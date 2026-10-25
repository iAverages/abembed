mod analytics;
mod config;
mod embeds;

use self::analytics::Analytics;
use self::config::{MachinaConfig, get_config};
use self::embeds::cache_manager::{MetadataCache, VideoCache};
use self::embeds::image_client::EmbedImageClient;
use self::embeds::preview::{
    get_album_page, get_episode_page, get_fallback_redirect, get_generated_album_image,
    get_generated_image, get_generated_playlist_image, get_playlist_page, get_preview_album_video,
    get_preview_playlist_video, get_preview_video, get_track_page,
};
use self::embeds::preview_generation::PreviewGeneration;
use self::embeds::renderer::FfmpegRenderer;
use self::embeds::spotify_metadata::SpotifyMetadataClient;
use self::embeds::video_source::B2VideoSource;
use axum::Router;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::routing::get;
use axum_tracing_opentelemetry::middleware::{OtelAxumLayer, OtelInResponseLayer};
use backblaze_b2_client::client::B2Client;
use init_tracing_opentelemetry::{LogTimer, TracingConfig};
use once_cell::sync::{Lazy, OnceCell};
use reqwest::Method;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::{self};
use tokio::signal;
use tokio::task;
use tower_http::cors::CorsLayer;
use tower_http::trace::{DefaultOnRequest, DefaultOnResponse, TraceLayer};
use tracing::Level;

#[derive(Clone)]
struct AppState {
    app_url: String,
    analytics: Arc<Analytics>,
    preview_generation: Arc<PreviewGeneration>,
    spotify_metadata: Arc<SpotifyMetadataClient>,
    image_client: Arc<EmbedImageClient>,
}

static B2: OnceCell<B2Client> = OnceCell::new();

pub fn get_b2() -> &'static B2Client {
    B2.get().expect("b2 client not set")
}

static MACHINA_CONFIG: Lazy<MachinaConfig> = Lazy::new(|| get_config().expect("config"));

#[tokio::main]
async fn main() {
    let tracing_config = if cfg!(debug_assertions) {
        TracingConfig::development()
            .with_compact_format()
            .with_log_directives("api=debug")
            .with_timer(LogTimer::None)
            .with_target_display(false)
            .with_file_names(false)
            .with_line_numbers(false)
            .with_thread_names(false)
            .with_thread_ids(false)
            .without_span_events()
    } else {
        TracingConfig::production()
    };
    let _guard = tracing_config.init_subscriber().expect("init tracing");
    let b2 = B2Client::new(
        MACHINA_CONFIG.b2_application_key_id.clone(),
        MACHINA_CONFIG.b2_application_key.clone(),
    )
    .await
    .expect("b2");

    ensure_video_generator_dir_exists_or_exit().await;

    let _ = B2.set(b2);

    let spotify_metadata = Arc::new(SpotifyMetadataClient::new(
        MetadataCache::new(MACHINA_CONFIG.spotify_metadata_cache_max_bytes),
        Duration::from_secs(MACHINA_CONFIG.spotify_playlist_metadata_ttl_secs),
    ));
    let image_client = Arc::new(EmbedImageClient::new());
    let renderer = Arc::new(FfmpegRenderer::new(
        MACHINA_CONFIG.video_generator_dir.clone(),
    ));
    let video_source = Arc::new(B2VideoSource::new());
    let analytics = Arc::new(
        Analytics::new(
            &MACHINA_CONFIG.posthog_api_key,
            &MACHINA_CONFIG.posthog_host,
        )
        .await,
    );
    let cache = Arc::new(VideoCache::new(
        MACHINA_CONFIG.video_cache_max_bytes,
        analytics.clone(),
    ));

    renderer
        .ensure_output_root_exists()
        .await
        .expect("video output root");

    let preview_generation = Arc::new(PreviewGeneration::new(
        cache.clone(),
        spotify_metadata.clone(),
        image_client.clone(),
        renderer.clone(),
        video_source.clone(),
        analytics.clone(),
    ));

    let state = AppState {
        app_url: MACHINA_CONFIG.app_url.clone(),
        analytics: analytics.clone(),
        preview_generation,
        spotify_metadata,
        image_client,
    };

    task::spawn(cache_upload_existing_tmp(video_source, renderer));

    let cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST])
        .allow_origin(
            MACHINA_CONFIG
                .app_url
                .trim_end_matches('/')
                .parse::<HeaderValue>()
                .unwrap(),
        );

    let app = Router::new()
        .route("/health", get(get_health))
        .route(
            "/api/generate/video/album/{albumId}",
            get(get_preview_album_video),
        )
        .route(
            "/api/generate/video/playlist/{playlistId}",
            get(get_preview_playlist_video),
        )
        .route("/api/generate/video/{trackId}", get(get_preview_video))
        .route("/api/generate/image/{trackId}", get(get_generated_image))
        .route(
            "/api/generate/image/album/{albumId}",
            get(get_generated_album_image),
        )
        .route(
            "/api/generate/image/playlist/{playlistId}",
            get(get_generated_playlist_image),
        )
        .route("/track/{trackId}", get(get_track_page))
        .route("/episode/{episodeId}", get(get_episode_page))
        .route("/album/{albumId}", get(get_album_page))
        .route("/playlist/{playlistId}", get(get_playlist_page))
        .fallback(get_fallback_redirect)
        .layer(
            TraceLayer::new_for_http()
                .on_request(DefaultOnRequest::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        .layer(OtelInResponseLayer)
        .layer(OtelAxumLayer::default())
        .layer(cors)
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    tracing::info!("listening on :3000");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();

    analytics.shutdown().await;
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("shutdown signal received");
}

async fn get_health() -> StatusCode {
    StatusCode::OK
}

async fn ensure_video_generator_dir_exists_or_exit() {
    if let Err(error) = fs::create_dir_all(&MACHINA_CONFIG.video_generator_dir).await {
        let already_exists = fs::metadata(&MACHINA_CONFIG.video_generator_dir)
            .await
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false);

        if !already_exists {
            tracing::error!(
                "failed to ensure video_generator_dir exists at {}: {}",
                MACHINA_CONFIG.video_generator_dir,
                error,
            );
            std::process::exit(1);
        }
    }
}

async fn cache_upload_existing_tmp(
    video_source: Arc<B2VideoSource>,
    renderer: Arc<FfmpegRenderer>,
) {
    tracing::debug!(
        "checking {} for existing generated videos",
        MACHINA_CONFIG.video_generator_dir
    );
    let mut entries = fs::read_dir(MACHINA_CONFIG.video_generator_dir.clone())
        .await
        .unwrap();
    while let Some(entry) = entries.next_entry().await.unwrap() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let Some(folder_name) = path.file_name() else {
            continue;
        };

        if let Some(track_id) = folder_name.to_str() {
            let video_path = renderer.video_output_path(track_id);
            if let Ok(video_bytes) = fs::read(video_path).await {
                let bytes = bytes::Bytes::from(video_bytes);
                if video_source.has_video(track_id).await.ok() != Some(true) {
                    let _ = video_source.upload_video_bytes(track_id, bytes).await;
                }
            }

            let _ = fs::remove_dir_all(renderer.track_output_path(track_id)).await;
            tracing::debug!("deleted tmp directory for {}", track_id);
        }
    }
}
