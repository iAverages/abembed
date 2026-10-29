use axum::extract::{Path, Query, RawQuery, State};
use axum::http::Uri;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html as AxumHtml, IntoResponse, Redirect, Response};
use bytes::Bytes;
use serde::Deserialize;

use crate::AppState;
use crate::embeds::preview_generation::{CacheStatus, PreloadedPreviewInput, normalize_track_id};
use crate::embeds::spotify_metadata::{
    SpotifyCollectionKind, SpotifyCollectionTrackMetadata, SpotifyPreviewMetadata,
};

#[derive(Deserialize)]
pub struct CollectionTrackQuery {
    track: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackQuery {
    #[serde(default)]
    disable_music_video: bool,
}

#[axum::debug_handler]
pub async fn get_album_page(
    Path(collection_id): Path<String>,
    Query(query): Query<CollectionTrackQuery>,
    RawQuery(raw_query): RawQuery,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    get_collection_page(
        SpotifyCollectionKind::Album,
        collection_id,
        query,
        raw_query,
        state,
        headers,
    )
    .await
}

#[axum::debug_handler]
pub async fn get_playlist_page(
    Path(collection_id): Path<String>,
    Query(query): Query<CollectionTrackQuery>,
    RawQuery(raw_query): RawQuery,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    get_collection_page(
        SpotifyCollectionKind::Playlist,
        collection_id,
        query,
        raw_query,
        state,
        headers,
    )
    .await
}

async fn get_collection_page(
    collection_kind: SpotifyCollectionKind,
    collection_id: String,
    query: CollectionTrackQuery,
    raw_query: Option<String>,
    state: AppState,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    tracing::info!(
        collection_kind = collection_kind.path_segment(),
        collection_id = collection_id.as_str(),
        requested_track = query.track.as_deref().unwrap_or(""),
        "collection page request received"
    );
    if !is_discord_request(&headers) {
        tracing::info!(
            collection_kind = collection_kind.path_segment(),
            collection_id = collection_id.as_str(),
            "collection page redirecting to spotify"
        );
        let redirect_url = spotify_collection_url(collection_kind, &collection_id);
        state
            .analytics
            .spotify_redirect(collection_kind.path_segment(), "collection_page");
        return Ok(spotify_open_response(&redirect_url));
    }

    let collection_data = state
        .spotify_metadata
        .get_collection_track_metadata(collection_kind, &collection_id, query.track.as_deref())
        .await
        .map_err(map_preview_error)?;

    let og = build_collection_og_image(&state, &collection_data, collection_kind, &collection_id)
        .await
        .map_err(map_preview_error)?;

    let title = if collection_data.collection_name.trim().is_empty() {
        collection_data.track.song_name.clone()
    } else {
        format!(
            "{} - {}",
            collection_data.track.song_name, collection_data.collection_name
        )
    };

    ensure_collection_video(
        &state,
        collection_kind,
        &collection_id,
        &collection_data,
        og.image_bytes,
    )
    .await?;

    let canonical_path = format!("/{}/{}", collection_kind.path_segment(), collection_id);
    let collection_image_url = append_query(
        format!(
            "{}/api/generate/image/{}/{}?track={}&v={}",
            state.app_url.trim_end_matches('/'),
            collection_kind.path_segment(),
            collection_id,
            collection_data.selected_track_index + 1,
            collection_data.content_fingerprint,
        ),
        raw_query.as_deref(),
    );
    let collection_video_url = append_query(
        format!(
            "{}/api/generate/video/{}/{}.mp4?track={}&v={}",
            state.app_url.trim_end_matches('/'),
            collection_kind.path_segment(),
            collection_id,
            collection_data.selected_track_index + 1,
            collection_data.content_fingerprint,
        ),
        raw_query.as_deref(),
    );
    let component_subtitle = build_component_subtitle(&collection_data.track, true);
    let block = build_preview_meta_page(
        &title,
        ComponentEmbedDetails {
            subtitle: &component_subtitle,
            thumbnail_url: collection_data.track.artist_image_url.as_deref(),
        },
        &canonical_path,
        &collection_video_url,
        &og.theme_color,
        &state.app_url,
        &collection_image_url,
    );

    Ok(AxumHtml(block).into_response())
}

#[axum::debug_handler]
pub async fn get_track_page(
    Path(track_id): Path<String>,
    Query(query): Query<TrackQuery>,
    RawQuery(raw_query): RawQuery,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    tracing::info!(track_id = track_id.as_str(), "track page request received");
    if !is_discord_request(&headers) {
        tracing::info!(
            track_id = track_id.as_str(),
            "track page redirecting to spotify"
        );
        let redirect_url = format!("https://open.spotify.com/track/{track_id}");
        state.analytics.spotify_redirect("track", "track_page");
        return Ok(spotify_open_response(&redirect_url));
    }

    let spotify_data = state
        .spotify_metadata
        .get_track_metadata(&track_id)
        .await
        .map_err(map_preview_error)?;

    let og = state
        .image_client
        .generate_track_og(&spotify_data)
        .await
        .map_err(map_preview_error)?;
    let component_subtitle = build_component_subtitle(&spotify_data, true);

    // if track has music video, use that instead of genearting a custom video
    if !query.disable_music_video
        && let Some(ref preview_video) = spotify_data.preview_video
    {
        let canonical_path = format!("/track/{track_id}");

        let block = build_preview_meta_page(
            &spotify_data.song_name,
            ComponentEmbedDetails {
                subtitle: &component_subtitle,
                thumbnail_url: Some(&spotify_data.album_art_url),
            },
            &canonical_path,
            &preview_video.video_url,
            &og.theme_color,
            &state.app_url,
            &preview_video.thumbnail_url,
        );

        return Ok(AxumHtml(block).into_response());
    }

    state
        .preview_generation
        .ensure_generated(PreloadedPreviewInput {
            track_id: spotify_data.media_id.clone(),
            video_kind: spotify_data.video_kind,
            preview_url: spotify_data.preview_audio_url.clone(),
            og_bytes: og.image_bytes,
        })
        .await
        .map_err(map_preview_error)?;

    let canonical_path = format!("/track/{track_id}");
    let image_url = append_query(
        format!(
            "{}/api/generate/image/{}",
            state.app_url.trim_end_matches('/'),
            spotify_data.media_id
        ),
        raw_query.as_deref(),
    );
    let video_url = append_query(
        format!(
            "{}/api/generate/video/{}.mp4",
            state.app_url.trim_end_matches('/'),
            spotify_data.media_id
        ),
        raw_query.as_deref(),
    );
    let block = build_preview_meta_page(
        &spotify_data.song_name,
        ComponentEmbedDetails {
            subtitle: &component_subtitle,
            thumbnail_url: spotify_data.artist_image_url.as_deref(),
        },
        &canonical_path,
        &video_url,
        &og.theme_color,
        &state.app_url,
        &image_url,
    );

    Ok(AxumHtml(block).into_response())
}

#[axum::debug_handler]
pub async fn get_episode_page(
    Path(episode_id): Path<String>,
    RawQuery(raw_query): RawQuery,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    tracing::info!(
        episode_id = episode_id.as_str(),
        "episode page request received"
    );
    if !is_discord_request(&headers) {
        tracing::info!(
            episode_id = episode_id.as_str(),
            "episode page redirecting to spotify"
        );
        let redirect_url = format!("https://open.spotify.com/episode/{episode_id}");
        state.analytics.spotify_redirect("episode", "episode_page");
        return Ok(spotify_open_response(&redirect_url));
    }

    let spotify_data = state
        .spotify_metadata
        .get_episode_metadata(&episode_id)
        .await
        .map_err(map_preview_error)?;

    let og = state
        .image_client
        .generate_track_og(&spotify_data)
        .await
        .map_err(map_preview_error)?;

    state
        .preview_generation
        .ensure_generated(PreloadedPreviewInput {
            track_id: spotify_data.media_id.clone(),
            video_kind: spotify_data.video_kind,
            preview_url: spotify_data.preview_audio_url.clone(),
            og_bytes: og.image_bytes,
        })
        .await
        .map_err(map_preview_error)?;

    let canonical_path = format!("/episode/{episode_id}");
    let title = if spotify_data.artist_names.is_empty() {
        spotify_data.song_name.clone()
    } else {
        format!(
            "{} - {}",
            spotify_data.song_name,
            spotify_data.artist_text()
        )
    };
    let component_subtitle = build_component_subtitle(&spotify_data, false);
    let image_url = append_query(
        format!(
            "{}/api/generate/image/{}",
            state.app_url.trim_end_matches('/'),
            spotify_data.media_id
        ),
        raw_query.as_deref(),
    );
    let video_url = append_query(
        format!(
            "{}/api/generate/video/{}.mp4",
            state.app_url.trim_end_matches('/'),
            spotify_data.media_id
        ),
        raw_query.as_deref(),
    );
    let block = build_preview_meta_page(
        &title,
        ComponentEmbedDetails {
            subtitle: &component_subtitle,
            thumbnail_url: None,
        },
        &canonical_path,
        &video_url,
        &og.theme_color,
        &state.app_url,
        &image_url,
    );

    Ok(AxumHtml(block).into_response())
}

#[axum::debug_handler]
pub async fn get_fallback_redirect(State(state): State<AppState>, uri: Uri) -> impl IntoResponse {
    tracing::info!(
        path = uri.path(),
        query = uri.query().unwrap_or(""),
        "fallback redirect requested"
    );
    state
        .analytics
        .spotify_redirect(fallback_media_type(&uri), "fallback");
    spotify_open_response(&build_passthrough_redirect(
        &uri,
        "",
        "https://open.spotify.com",
    ))
}

#[axum::debug_handler]
pub async fn get_generated_image(
    Path(track_id): Path<String>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    tracing::info!(
        track_id = track_id.as_str(),
        "generated image request received"
    );
    let spotify_data = state
        .spotify_metadata
        .get_track_or_episode_metadata(&track_id)
        .await
        .map_err(map_preview_error)?;

    let og = state
        .image_client
        .generate_track_og(&spotify_data)
        .await
        .map_err(map_preview_error)?;

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "image/png".parse().unwrap());
    headers.insert("X-Basecolor", og.theme_color.parse().unwrap());

    Ok((StatusCode::OK, headers, og.image_bytes).into_response())
}

#[axum::debug_handler]
pub async fn get_generated_album_image(
    Path(collection_id): Path<String>,
    Query(query): Query<CollectionTrackQuery>,
    State(state): State<AppState>,
) -> Result<Response, (StatusCode, String)> {
    get_generated_collection_image(SpotifyCollectionKind::Album, collection_id, query, state).await
}

#[axum::debug_handler]
pub async fn get_generated_playlist_image(
    Path(collection_id): Path<String>,
    Query(query): Query<CollectionTrackQuery>,
    State(state): State<AppState>,
) -> Result<Response, (StatusCode, String)> {
    get_generated_collection_image(SpotifyCollectionKind::Playlist, collection_id, query, state)
        .await
}

async fn get_generated_collection_image(
    collection_kind: SpotifyCollectionKind,
    collection_id: String,
    query: CollectionTrackQuery,
    state: AppState,
) -> Result<Response, (StatusCode, String)> {
    tracing::info!(
        collection_kind = collection_kind.path_segment(),
        collection_id = collection_id.as_str(),
        requested_track = query.track.as_deref().unwrap_or(""),
        "generated collection image request received"
    );
    let collection_data = state
        .spotify_metadata
        .get_collection_track_metadata(collection_kind, &collection_id, query.track.as_deref())
        .await
        .map_err(map_preview_error)?;

    let og = build_collection_og_image(&state, &collection_data, collection_kind, &collection_id)
        .await
        .map_err(map_preview_error)?;

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "image/png".parse().unwrap());
    headers.insert("X-Basecolor", og.theme_color.parse().unwrap());

    Ok((StatusCode::OK, headers, og.image_bytes).into_response())
}

#[axum::debug_handler]
pub async fn get_preview_video(
    Path(track_id): Path<String>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    tracing::info!(
        track_id = track_id.as_str(),
        "preview video request received"
    );
    let served = state
        .preview_generation
        .ensure_and_serve(&track_id)
        .await
        .map_err(map_preview_error)?;
    tracing::debug!(
        track_id = track_id.as_str(),
        cache_status = cache_status_label(&served.cache_status),
        "preview served"
    );
    Ok(build_video_response(served.video_bytes, served.mime))
}

#[axum::debug_handler]
pub async fn get_preview_album_video(
    Path(collection_id): Path<String>,
    Query(query): Query<CollectionTrackQuery>,
    State(state): State<AppState>,
) -> Result<Response, (StatusCode, String)> {
    get_preview_collection_video(SpotifyCollectionKind::Album, collection_id, query, state).await
}

#[axum::debug_handler]
pub async fn get_preview_playlist_video(
    Path(collection_id): Path<String>,
    Query(query): Query<CollectionTrackQuery>,
    State(state): State<AppState>,
) -> Result<Response, (StatusCode, String)> {
    get_preview_collection_video(SpotifyCollectionKind::Playlist, collection_id, query, state).await
}

async fn get_preview_collection_video(
    collection_kind: SpotifyCollectionKind,
    collection_id: String,
    query: CollectionTrackQuery,
    state: AppState,
) -> Result<Response, (StatusCode, String)> {
    let collection_id = normalize_track_id(&collection_id).map_err(map_preview_error)?;
    tracing::info!(
        collection_kind = collection_kind.path_segment(),
        collection_id = collection_id.as_str(),
        requested_track = query.track.as_deref().unwrap_or(""),
        "collection preview video request received"
    );

    let collection_data = state
        .spotify_metadata
        .get_collection_track_metadata(collection_kind, &collection_id, query.track.as_deref())
        .await
        .map_err(map_preview_error)?;

    let og = build_collection_og_image(&state, &collection_data, collection_kind, &collection_id)
        .await
        .map_err(map_preview_error)?;

    let collection_video_id = ensure_collection_video(
        &state,
        collection_kind,
        &collection_id,
        &collection_data,
        og.image_bytes,
    )
    .await?;

    let served = state
        .preview_generation
        .ensure_and_serve(&collection_video_id)
        .await
        .map_err(map_preview_error)?;

    Ok(build_video_response(served.video_bytes, served.mime))
}

fn cache_status_label(cache_status: &CacheStatus) -> &'static str {
    match cache_status {
        CacheStatus::Hit => "hit",
        CacheStatus::Hydrated => "hydrated",
        CacheStatus::Rendered => "rendered",
    }
}

fn build_video_response(video_bytes: Bytes, mime: &str) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
    headers.insert(header::CONTENT_TYPE, mime.parse().unwrap());
    (StatusCode::OK, headers, video_bytes).into_response()
}

pub fn build_passthrough_redirect(uri: &Uri, route_prefix: &str, target_base: &str) -> String {
    let path = uri
        .path()
        .strip_prefix(route_prefix)
        .unwrap_or(uri.path())
        .trim_start_matches('/');

    let mut redirect_url = if path.is_empty() {
        format!("{}/", target_base.trim_end_matches('/'))
    } else {
        format!("{}/{}", target_base.trim_end_matches('/'), path)
    };

    if let Some(query) = uri.query() {
        redirect_url.push('?');
        redirect_url.push_str(query);
    }

    redirect_url
}

fn spotify_open_response(web_url: &str) -> Response {
    let Some(app_uri) = spotify_app_uri(web_url) else {
        return Redirect::temporary(web_url).into_response();
    };

    let block = include_str!("spotify_open.html")
        .replace("__WEB_ATTR__", &escape_html_attribute(web_url))
        .replace(
            "__WEB_JS__",
            &serde_json::to_string(web_url).expect("string serializes"),
        )
        .replace(
            "__APP_URI_JS__",
            &serde_json::to_string(&app_uri).expect("string serializes"),
        );

    AxumHtml(block).into_response()
}

fn spotify_app_uri(web_url: &str) -> Option<String> {
    let url = reqwest::Url::parse(web_url).ok()?;
    let mut segments = url.path_segments()?.filter(|segment| !segment.is_empty());
    let mut kind = segments.next()?;
    if kind.starts_with("intl-") {
        kind = segments.next()?;
    }
    let id = segments.next()?;
    if segments.next().is_some()
        || !matches!(
            kind,
            "track" | "album" | "playlist" | "episode" | "artist" | "show"
        )
        || !id.chars().all(|c| c.is_ascii_alphanumeric())
    {
        return None;
    }
    Some(format!("spotify:{kind}:{id}"))
}

fn is_discord_request(headers: &HeaderMap) -> bool {
    headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(|ua| ua.to_ascii_lowercase().contains("discord"))
        .unwrap_or(false)
}

fn fallback_media_type(uri: &Uri) -> &str {
    uri.path()
        .trim_start_matches('/')
        .split('/')
        .next()
        .filter(|segment| matches!(*segment, "track" | "episode" | "album" | "playlist"))
        .unwrap_or("other")
}

fn map_preview_error(error: impl std::fmt::Display) -> (StatusCode, String) {
    tracing::error!("preview pipeline error: {error}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "failed to generate preview".to_string(),
    )
}

async fn ensure_collection_video(
    state: &AppState,
    collection_kind: SpotifyCollectionKind,
    collection_id: &str,
    collection_data: &SpotifyCollectionTrackMetadata,
    og_bytes: Bytes,
) -> Result<String, (StatusCode, String)> {
    let collection_video_id = build_collection_video_id(
        collection_kind,
        collection_id,
        collection_data.selected_track_index,
        &collection_data.track.media_id,
        &collection_data.content_fingerprint,
    );

    let generation = state
        .preview_generation
        .ensure_generated(PreloadedPreviewInput {
            track_id: collection_video_id.clone(),
            video_kind: collection_kind.video_kind(),
            preview_url: collection_data.track.preview_audio_url.clone(),
            og_bytes,
        })
        .await
        .map_err(map_preview_error)?;

    if matches!(generation, CacheStatus::Rendered) {
        state
            .preview_generation
            .invalidate_stale_variants(
                &collection_video_id_prefix(collection_kind, collection_id),
                &collection_video_variant_suffix(&collection_data.content_fingerprint),
            )
            .await;
    }

    Ok(collection_video_id)
}

fn build_collection_video_id(
    collection_kind: SpotifyCollectionKind,
    collection_id: &str,
    selected_track_index: usize,
    media_track_id: &str,
    content_fingerprint: &str,
) -> String {
    format!(
        "{}{selected_track_index}-{media_track_id}{}",
        collection_video_id_prefix(collection_kind, collection_id),
        collection_video_variant_suffix(content_fingerprint),
    )
}

fn collection_video_id_prefix(
    collection_kind: SpotifyCollectionKind,
    collection_id: &str,
) -> String {
    format!("{}-{collection_id}-", collection_kind.path_segment())
}

fn collection_video_variant_suffix(content_fingerprint: &str) -> String {
    format!("-{content_fingerprint}")
}

fn spotify_collection_url(collection_kind: SpotifyCollectionKind, collection_id: &str) -> String {
    format!(
        "https://open.spotify.com/{}/{collection_id}",
        collection_kind.path_segment()
    )
}

fn append_query(url: String, query: Option<&str>) -> String {
    let Some(query) = query.filter(|query| !query.is_empty()) else {
        return url;
    };

    let mut url = reqwest::Url::parse(&url).expect("internally generated media URL must be valid");
    let system_params = url
        .query_pairs()
        .map(|(key, _)| key.into_owned())
        .collect::<Vec<_>>();
    let external_url = reqwest::Url::parse(&format!("https://query.invalid/?{query}"))
        .expect("request query string must be valid");
    let external_params = external_url
        .query_pairs()
        .filter(|(key, _)| !system_params.iter().any(|system_key| system_key == key))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();

    url.query_pairs_mut().extend_pairs(external_params);
    url.into()
}

async fn build_collection_og_image(
    state: &AppState,
    collection_data: &SpotifyCollectionTrackMetadata,
    collection_kind: SpotifyCollectionKind,
    collection_id: &str,
) -> Result<crate::embeds::image_client::GeneratedOgImage, crate::embeds::image_client::OgImageError>
{
    if collection_data
        .track_names
        .iter()
        .all(|name| name.trim().is_empty())
    {
        return state
            .image_client
            .generate_track_og(&collection_data.track)
            .await;
    }

    match state
        .image_client
        .generate_collection_og(collection_kind, collection_data)
        .await
    {
        Ok(image) => Ok(image),
        Err(error) => {
            tracing::warn!(
                "falling back to single-track image for {} {}: {}",
                collection_kind.path_segment(),
                collection_id,
                error
            );
            state
                .image_client
                .generate_track_og(&collection_data.track)
                .await
        }
    }
}

struct ComponentEmbedDetails<'a> {
    subtitle: &'a str,
    thumbnail_url: Option<&'a str>,
}

fn build_preview_meta_page(
    title: &str,
    component_details: ComponentEmbedDetails<'_>,
    canonical_path: &str,
    video_url: &str,
    theme_color: &str,
    app_url: &str,
    image_url: &str,
) -> String {
    let app_url = app_url.trim_end_matches('/');
    let canonical_url = format!("{app_url}{canonical_path}");
    let component_embed = build_component_embed_json(
        title,
        component_details.subtitle,
        &canonical_url,
        video_url,
        theme_color,
        component_details.thumbnail_url,
    );
    let canonical_url = escape_html_attribute(&canonical_url);
    let title = escape_html_attribute(title);
    let video_url = escape_html_attribute(video_url);
    let image_url = escape_html_attribute(image_url);
    format!(
        concat!(
            "<!doctype html>",
            "<html lang=\"en\">",
            "<head>",
            "<title>{title}</title>",
            "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">",
            "<meta property=\"og:title\" content=\"{title}\">",
            "<meta property=\"og:url\" content=\"{canonical_url}\">",
            "<meta property=\"theme-color\" content=\"{theme_color}\">",
            "<meta property=\"og:image\" content=\"{image_url}\">",
            "<meta property=\"og:type\" content=\"video\">",
            "<meta property=\"og:video\" content=\"{video_url}\">",
            "<meta property=\"og:video:type\" content=\"video/mp4\">",
            "<meta property=\"og:video:height\" content=\"300\">",
            "<meta property=\"og:video:width\" content=\"800\">",
            "<meta property=\"og:video:secure_url\" content=\"{video_url}\">",
            "<meta name=\"twitter:card\" content=\"summary_large_image\">",
            "<meta name=\"twitter:title\" content=\"{title}\">",
            "<meta name=\"twitter:image\" content=\"{image_url}\">",
            "<script id=\"discord:component-embed\" type=\"application/json\">{component_embed}</script>",
            "</head>",
            "<body></body>",
            "</html>"
        ),
        title = title,
        canonical_url = canonical_url,
        video_url = video_url,
        theme_color = theme_color,
        image_url = image_url,
        component_embed = component_embed,
    )
}

fn build_component_embed_json(
    title: &str,
    subtitle: &str,
    page_url: &str,
    video_url: &str,
    theme_color: &str,
    thumbnail_url: Option<&str>,
) -> String {
    let content = if subtitle.is_empty() {
        format!("# [{title}]({page_url})")
    } else {
        format!("# [{title}]({page_url})\n{subtitle}")
    };
    let heading = if let Some(thumbnail_url) = thumbnail_url {
        serde_json::json!({
            "type": 9,
            "components": [{"type": 10, "content": content}],
            "accessory": {
                "type": 11,
                "media": {"url": thumbnail_url},
                "description": "Artist image",
            },
        })
    } else {
        serde_json::json!({"type": 10, "content": content})
    };

    let mut component = serde_json::json!({
        "type": 17,
        "components": [
            heading,
            {
                "type": 12,
                "items": [{
                    "media": {
                        "url": video_url
                    }
                }]
            },
        ],
    });
    if let Some(accent_color) = theme_color
        .strip_prefix('#')
        .filter(|value| value.len() == 6)
        .and_then(|value| u32::from_str_radix(value, 16).ok())
    {
        component["accent_color"] = accent_color.into();
    }

    serde_json::json!({"component": component})
        .to_string()
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
}

fn build_component_subtitle(metadata: &SpotifyPreviewMetadata, include_artist: bool) -> String {
    let mut details = Vec::new();
    if include_artist {
        details.push(metadata.artist_text());
    }

    if let Some(duration_ms) = metadata.duration_ms.filter(|duration| *duration >= 0) {
        let seconds = duration_ms / 1_000;
        details.push(format!("`{}:{:02}`", seconds / 60, seconds % 60));
    }
    if let Some(release_date) = metadata.release_date.as_deref() {
        let release_date = chrono::DateTime::parse_from_rfc3339(release_date)
            .map(|date| date.format("%B %-d, %Y").to_string())
            .unwrap_or_else(|_| release_date.to_string());
        details.push(release_date);
    }

    details.join(" · ")
}

fn escape_html_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::{
        ComponentEmbedDetails, append_query, build_collection_video_id, build_component_embed_json,
        build_component_subtitle, build_passthrough_redirect, build_preview_meta_page,
        collection_video_id_prefix, collection_video_variant_suffix, fallback_media_type,
        spotify_app_uri, spotify_collection_url,
    };
    use crate::embeds::VideoKind;
    use crate::embeds::spotify_metadata::{SpotifyCollectionKind, SpotifyPreviewMetadata};
    use axum::http::Uri;

    #[test]
    fn spotify_app_uri_maps_known_media_urls() {
        assert_eq!(
            spotify_app_uri("https://open.spotify.com/track/abc123?si=xyz").as_deref(),
            Some("spotify:track:abc123")
        );
        assert_eq!(
            spotify_app_uri("https://open.spotify.com/intl-de/album/abc123").as_deref(),
            Some("spotify:album:abc123")
        );
    }

    #[test]
    fn spotify_app_uri_rejects_unknown_or_nested_paths() {
        assert_eq!(spotify_app_uri("https://open.spotify.com/"), None);
        assert_eq!(spotify_app_uri("https://open.spotify.com/search/abc"), None);
        assert_eq!(
            spotify_app_uri("https://open.spotify.com/user/x/playlist/y"),
            None
        );
        assert_eq!(
            spotify_app_uri("https://open.spotify.com/track/abc<script>"),
            None
        );
    }

    #[test]
    fn passthrough_redirect_keeps_wildcard_and_query() {
        let uri: Uri = "/prerelease/track/abc123?si=xyz&foo=bar".parse().unwrap();

        let redirect = build_passthrough_redirect(&uri, "/prerelease", "https://open.spotify.com/");

        assert_eq!(
            redirect,
            "https://open.spotify.com/track/abc123?si=xyz&foo=bar"
        );
    }

    #[test]
    fn passthrough_redirect_handles_base_route() {
        let uri: Uri = "/prerelease?foo=bar".parse().unwrap();

        let redirect = build_passthrough_redirect(&uri, "/prerelease", "https://open.spotify.com");

        assert_eq!(redirect, "https://open.spotify.com/?foo=bar");
    }

    #[test]
    fn passthrough_redirect_handles_empty_prefix() {
        let uri: Uri = "/track/abc123?si=xyz".parse().unwrap();

        let redirect = build_passthrough_redirect(&uri, "", "https://open.spotify.com");

        assert_eq!(redirect, "https://open.spotify.com/track/abc123?si=xyz");
    }

    #[test]
    fn fallback_redirect_classifies_known_spotify_media_types() {
        assert_eq!(
            fallback_media_type(&"/album/abc123".parse::<Uri>().unwrap()),
            "album"
        );
        assert_eq!(
            fallback_media_type(&"/artist/abc123".parse::<Uri>().unwrap()),
            "other"
        );
    }

    #[test]
    fn playlist_urls_point_to_spotify_playlists() {
        assert_eq!(
            spotify_collection_url(SpotifyCollectionKind::Playlist, "playlist-id"),
            "https://open.spotify.com/playlist/playlist-id"
        );
    }

    #[test]
    fn collection_video_ids_are_namespaced_by_kind() {
        assert_eq!(
            build_collection_video_id(SpotifyCollectionKind::Album, "id", 1, "track", "fp1"),
            "album-id-1-track-fp1"
        );
        assert_eq!(
            build_collection_video_id(SpotifyCollectionKind::Playlist, "id", 1, "track", "fp1"),
            "playlist-id-1-track-fp1"
        );
    }

    #[test]
    fn collection_video_ids_change_with_the_content_fingerprint() {
        let before =
            build_collection_video_id(SpotifyCollectionKind::Playlist, "id", 1, "track", "aaa");
        let after =
            build_collection_video_id(SpotifyCollectionKind::Playlist, "id", 1, "track", "bbb");
        assert_ne!(before, after);
    }

    #[test]
    fn collection_video_id_prefix_and_suffix_bracket_every_variant() {
        let prefix = collection_video_id_prefix(SpotifyCollectionKind::Playlist, "id");
        let suffix = collection_video_variant_suffix("fp1");
        for (index, track) in [(0, "t1"), (7, "t2")] {
            let id = build_collection_video_id(
                SpotifyCollectionKind::Playlist,
                "id",
                index,
                track,
                "fp1",
            );
            assert!(id.starts_with(&prefix), "{id} should start with {prefix}");
            assert!(id.ends_with(&suffix), "{id} should end with {suffix}");
        }
        assert!(
            !build_collection_video_id(SpotifyCollectionKind::Playlist, "id", 0, "t1", "fp0")
                .ends_with(&suffix)
        );
        assert!(
            !build_collection_video_id(SpotifyCollectionKind::Playlist, "id2", 0, "t1", "fp1")
                .starts_with(&prefix)
        );
    }

    #[test]
    fn media_url_keeps_external_query_params_without_overriding_system_params() {
        assert_eq!(
            append_query(
                "https://spqtify.com/api/generate/image/id?track=2".to_string(),
                Some("some=query&track=99&tr%61ck=100&cache=2")
            ),
            "https://spqtify.com/api/generate/image/id?track=2&some=query&cache=2"
        );
    }

    #[test]
    fn preview_meta_page_includes_query_params_in_image_and_video_urls() {
        let page = build_preview_meta_page(
            "Title",
            ComponentEmbedDetails {
                subtitle: "Artist",
                thumbnail_url: None,
            },
            "/track/id",
            "https://spqtify.com/api/generate/video/id.mp4?some=query&cache=2",
            "#000000",
            "https://spqtify.com",
            "https://spqtify.com/api/generate/image/id?some=query&cache=2",
        );

        assert!(page.contains(
            "<meta property=\"og:image\" content=\"https://spqtify.com/api/generate/image/id?some=query&amp;cache=2\">"
        ));
        assert!(page.contains(
            "<meta property=\"og:video\" content=\"https://spqtify.com/api/generate/video/id.mp4?some=query&amp;cache=2\">"
        ));
    }

    #[test]
    fn preview_meta_page_prevents_component_script_termination() {
        let page = build_preview_meta_page(
            "</script>",
            ComponentEmbedDetails {
                subtitle: "Artist",
                thumbnail_url: None,
            },
            "/track/id",
            "https://spqtify.com/video.mp4",
            "#000000",
            "https://spqtify.com",
            "https://spqtify.com/image.png",
        );

        assert_eq!(page.matches("</script>").count(), 1);
        assert!(page.contains(r#"\u003c/script\u003e"#));
    }

    #[test]
    fn component_embed_only_includes_artwork_when_requested() {
        let without_artwork = build_component_embed_json(
            "Title",
            "Artist",
            "https://spqtify.com/track/id",
            "https://spqtify.com/video.mp4",
            "#000000",
            None,
        );
        let with_artwork = build_component_embed_json(
            "Title",
            "Artist",
            "https://spqtify.com/track/id",
            "https://spqtify.com/video.mp4",
            "#000000",
            Some("https://spqtify.com/cover.jpg"),
        );

        assert!(!without_artwork.contains("cover.jpg"));
        assert!(with_artwork.contains("cover.jpg"));
    }

    #[test]
    fn component_embed_without_artwork_uses_text_display() {
        let payload: serde_json::Value = serde_json::from_str(&build_component_embed_json(
            "Title",
            "Artist · `2:17` · August 19, 2026",
            "https://spqtify.com/track/id",
            "https://spqtify.com/video.mp4",
            "#000000",
            None,
        ))
        .unwrap();

        assert_eq!(payload["component"]["components"][0]["type"], 10);
        assert_eq!(
            payload["component"]["components"][0]["content"],
            "# [Title](https://spqtify.com/track/id)\nArtist · `2:17` · August 19, 2026"
        );
    }

    #[test]
    fn component_subtitle_includes_duration_and_release_date() {
        let metadata = SpotifyPreviewMetadata {
            media_id: "id".to_string(),
            video_kind: VideoKind::Track,
            song_name: "Title".to_string(),
            artist_names: vec!["Artist".to_string()],
            preview_audio_url: "https://spqtify.com/audio.mp3".to_string(),
            preview_video: None,
            album_art_url: "https://spqtify.com/cover.jpg".to_string(),
            artist_image_url: None,
            duration_ms: Some(137_096),
            release_date: Some("2026-08-19T00:00:00Z".to_string()),
        };

        assert_eq!(
            build_component_subtitle(&metadata, true),
            "Artist · `2:17` · August 19, 2026"
        );
    }

    #[test]
    fn preview_meta_page_escapes_double_quotes_in_embed_title() {
        let page = build_preview_meta_page(
            "Song \"Live\" Version",
            ComponentEmbedDetails {
                subtitle: "Artist",
                thumbnail_url: None,
            },
            "/track/id",
            "https://spqtify.com/video.mp4",
            "#000000",
            "https://spqtify.com",
            "https://spqtify.com/image.png",
        );

        assert!(
            page.contains("<meta property=\"og:title\" content=\"Song &quot;Live&quot; Version\">")
        );
    }

    #[test]
    fn preview_meta_page_escapes_canonical_url() {
        let page = build_preview_meta_page(
            "Title",
            ComponentEmbedDetails {
                subtitle: "Artist",
                thumbnail_url: None,
            },
            "/track/id?foo=one&bar=\"two\"",
            "https://spqtify.com/video.mp4",
            "#000000",
            "https://spqtify.com",
            "https://spqtify.com/image.png",
        );

        assert!(page.contains(
            "<meta property=\"og:url\" content=\"https://spqtify.com/track/id?foo=one&amp;bar=&quot;two&quot;\">"
        ));
    }
}
