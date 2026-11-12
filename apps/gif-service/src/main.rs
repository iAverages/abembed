use axum::Router;
use axum::body::Body;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::Bytes;
use futures::StreamExt;
use reqwest::{Client, Url, redirect::Policy};
use std::env;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::time::timeout;
use tower_http::trace::TraceLayer;

const TWITTER_UPSTREAM: &str = "https://video.twimg.com/";
const REDDIT_PREVIEW_UPSTREAM: &str = "https://external-preview.redd.it/";
const CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
const DEFAULT_MAX_INPUT_BYTES: usize = 50 * 1024 * 1024;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 128 * 1024 * 1024;

#[derive(Clone, Copy)]
enum OutputFormat {
    Webp,
    Gif,
}

#[derive(Clone)]
struct AppState {
    client: Client,
    twitter_upstream: Url,
    reddit_preview_upstream: Url,
    conversions: Arc<Semaphore>,
    max_input_bytes: usize,
    max_output_bytes: usize,
    convert_timeout: Duration,
}

#[derive(Debug)]
enum ServiceError {
    UpstreamNotFound,
    Upstream,
    UpstreamTimeout,
    TooLarge,
    Busy,
    InvalidVideo,
    Internal,
}

impl IntoResponse for ServiceError {
    fn into_response(self) -> Response {
        tracing::warn!(error = ?self, "gif-service request failed");
        let (status, message) = match self {
            Self::UpstreamNotFound => (StatusCode::NOT_FOUND, "source video not found"),
            Self::Upstream => (StatusCode::BAD_GATEWAY, "failed to fetch source video"),
            Self::UpstreamTimeout => (StatusCode::GATEWAY_TIMEOUT, "source video timed out"),
            Self::TooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "video is too large"),
            Self::Busy => (
                StatusCode::SERVICE_UNAVAILABLE,
                "conversion capacity exhausted",
            ),
            Self::InvalidVideo => (StatusCode::UNPROCESSABLE_ENTITY, "failed to convert video"),
            Self::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "conversion service failed",
            ),
        };
        let mut response = (status, message).into_response();
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        if status == StatusCode::SERVICE_UNAVAILABLE {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        }
        response
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let fetch_timeout = env_value("GIF_SERVICE_FETCH_TIMEOUT_SECONDS", 30);
    let state = AppState {
        client: Client::builder()
            .user_agent(concat!("gif-service/", env!("CARGO_PKG_VERSION")))
            .redirect(Policy::none())
            .timeout(Duration::from_secs(fetch_timeout))
            .build()
            .expect("HTTP client"),
        twitter_upstream: Url::parse(TWITTER_UPSTREAM).expect("valid twitter upstream"),
        reddit_preview_upstream: Url::parse(REDDIT_PREVIEW_UPSTREAM)
            .expect("valid reddit preview upstream"),
        conversions: Arc::new(Semaphore::new(env_value("GIF_SERVICE_MAX_CONVERSIONS", 2))),
        max_input_bytes: env_value("GIF_SERVICE_MAX_INPUT_BYTES", DEFAULT_MAX_INPUT_BYTES),
        max_output_bytes: env_value("GIF_SERVICE_MAX_OUTPUT_BYTES", DEFAULT_MAX_OUTPUT_BYTES),
        convert_timeout: Duration::from_secs(env_value("GIF_SERVICE_CONVERT_TIMEOUT_SECONDS", 60)),
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/tweet_video/{filename}", get(convert_tweet_video))
        .route("/external-preview/{filename}", get(convert_reddit_preview))
        .layer(TraceLayer::new_for_http())
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3002")
        .await
        .expect("listen on port 3002");
    tracing::info!("listening on :3002");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("serve gif-service");
}

async fn health() -> Response {
    let mut response = StatusCode::OK.into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn convert_tweet_video(
    Path(filename): Path<String>,
    State(state): State<AppState>,
) -> Result<Response, ServiceError> {
    let source = source_url(
        &state.twitter_upstream,
        &["tweet_video", &upstream_filename(&filename, "mp4")],
        None,
    )?;
    convert(&state, source, &filename).await
}

async fn convert_reddit_preview(
    Path(filename): Path<String>,
    RawQuery(query): RawQuery,
    State(state): State<AppState>,
) -> Result<Response, ServiceError> {
    let source = source_url(
        &state.reddit_preview_upstream,
        &[&upstream_filename(&filename, "gif")],
        query.as_deref(),
    )?;
    convert(&state, source, &filename).await
}

async fn convert(state: &AppState, source: Url, filename: &str) -> Result<Response, ServiceError> {
    let output_format = if filename.ends_with(".gif") {
        OutputFormat::Gif
    } else {
        OutputFormat::Webp
    };
    let _permit = state
        .conversions
        .clone()
        .try_acquire_owned()
        .map_err(|_| ServiceError::Busy)?;
    let source = fetch_video(state, source).await?;
    let image = transcode(
        source,
        output_format,
        state.max_output_bytes,
        state.convert_timeout,
    )
    .await?;
    Ok(image_response(image, output_format))
}

fn upstream_filename(filename: &str, extension: &str) -> String {
    filename
        .strip_suffix(".webp")
        .or_else(|| filename.strip_suffix(".gif"))
        .map_or_else(|| filename.to_owned(), |name| format!("{name}.{extension}"))
}

fn source_url(upstream: &Url, path: &[&str], query: Option<&str>) -> Result<Url, ServiceError> {
    let mut url = upstream.clone();
    url.path_segments_mut()
        .map_err(|_| ServiceError::Internal)?
        .pop_if_empty()
        .extend(path);
    url.set_query(query);
    url.set_fragment(None);
    Ok(url)
}

async fn fetch_video(state: &AppState, source: Url) -> Result<Bytes, ServiceError> {
    let response = state.client.get(source).send().await.map_err(|error| {
        if error.is_timeout() {
            ServiceError::UpstreamTimeout
        } else {
            ServiceError::Upstream
        }
    })?;
    if response.status() == StatusCode::NOT_FOUND {
        return Err(ServiceError::UpstreamNotFound);
    }
    if !response.status().is_success() {
        return Err(ServiceError::Upstream);
    }
    let content_length = response.content_length();
    if content_length.is_some_and(|length| length > state.max_input_bytes as u64) {
        return Err(ServiceError::TooLarge);
    }

    let mut bytes = Vec::with_capacity(content_length.unwrap_or_default() as usize);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ServiceError::Upstream)?;
        if bytes.len() + chunk.len() > state.max_input_bytes {
            return Err(ServiceError::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes.into())
}

async fn transcode(
    input: Bytes,
    output_format: OutputFormat,
    max_output_bytes: usize,
    duration: Duration,
) -> Result<Bytes, ServiceError> {
    timeout(
        duration,
        transcode_inner(input, output_format, max_output_bytes),
    )
    .await
    .map_err(|_| ServiceError::InvalidVideo)?
}

async fn transcode_inner(
    input: Bytes,
    output_format: OutputFormat,
    max_output_bytes: usize,
) -> Result<Bytes, ServiceError> {
    if matches!(output_format, OutputFormat::Gif) {
        return transcode_gif(input, max_output_bytes).await;
    }

    let mut command = Command::new("ffmpeg");
    command.args(ffmpeg_args());
    command.args(["-c:v", "libwebp_anim", "-loop", "0", "-f", "webp", "pipe:1"]);
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| ServiceError::Internal)?;
    let mut stdin = child.stdin.take().ok_or(ServiceError::Internal)?;
    let stdout = child.stdout.take().ok_or(ServiceError::Internal)?;

    let write = async move {
        stdin
            .write_all(&input)
            .await
            .map_err(|_| ServiceError::InvalidVideo)?;
        stdin
            .shutdown()
            .await
            .map_err(|_| ServiceError::InvalidVideo)
    };
    let read = read_limited(stdout, max_output_bytes);
    let wait = async { child.wait().await.map_err(|_| ServiceError::Internal) };
    let (_, output, status) = tokio::try_join!(write, read, wait)?;
    if !status.success() || !is_image(&output, output_format) {
        return Err(ServiceError::InvalidVideo);
    }
    Ok(Bytes::from(output))
}

async fn transcode_gif(input: Bytes, max_output_bytes: usize) -> Result<Bytes, ServiceError> {
    let mut ffmpeg = Command::new("ffmpeg");
    ffmpeg
        .args(ffmpeg_args())
        .args(["-f", "yuv4mpegpipe", "pipe:1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut ffmpeg = ffmpeg.spawn().map_err(|_| ServiceError::Internal)?;
    let mut ffmpeg_stdin = ffmpeg.stdin.take().ok_or(ServiceError::Internal)?;
    let mut ffmpeg_stdout = ffmpeg.stdout.take().ok_or(ServiceError::Internal)?;

    let mut gifski = Command::new("gifski")
        .args(["--quiet", "--output", "-", "--fast", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| ServiceError::Internal)?;
    let mut gifski_stdin = gifski.stdin.take().ok_or(ServiceError::Internal)?;
    let gifski_stdout = gifski.stdout.take().ok_or(ServiceError::Internal)?;

    let write = async move {
        ffmpeg_stdin
            .write_all(&input)
            .await
            .map_err(|_| ServiceError::InvalidVideo)?;
        ffmpeg_stdin
            .shutdown()
            .await
            .map_err(|_| ServiceError::InvalidVideo)
    };
    let pipe = async move {
        tokio::io::copy(&mut ffmpeg_stdout, &mut gifski_stdin)
            .await
            .map_err(|_| ServiceError::InvalidVideo)?;
        gifski_stdin
            .shutdown()
            .await
            .map_err(|_| ServiceError::InvalidVideo)
    };
    let read = read_limited(gifski_stdout, max_output_bytes);
    let wait_ffmpeg = async { ffmpeg.wait().await.map_err(|_| ServiceError::Internal) };
    let wait_gifski = async { gifski.wait().await.map_err(|_| ServiceError::Internal) };
    let (_, _, output, ffmpeg_status, gifski_status) =
        tokio::try_join!(write, pipe, read, wait_ffmpeg, wait_gifski)?;
    if !ffmpeg_status.success() || !gifski_status.success() || !is_image(&output, OutputFormat::Gif)
    {
        return Err(ServiceError::InvalidVideo);
    }
    Ok(Bytes::from(output))
}

async fn read_limited(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> Result<Vec<u8>, ServiceError> {
    let mut output = Vec::new();
    let mut buffer = [0; 16 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|_| ServiceError::Internal)?;
        if read == 0 {
            return Ok(output);
        }
        if output.len() + read > limit {
            return Err(ServiceError::TooLarge);
        }
        output.extend_from_slice(&buffer[..read]);
    }
}

fn ffmpeg_args() -> [&'static str; 12] {
    [
        "-hide_banner",
        "-loglevel",
        "error",
        "-i",
        "pipe:0",
        "-map",
        "0:v:0",
        "-an",
        "-sn",
        "-dn",
        "-fps_mode",
        "passthrough",
    ]
}

fn is_image(bytes: &[u8], output_format: OutputFormat) -> bool {
    match output_format {
        OutputFormat::Webp => {
            bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP"
        }
        OutputFormat::Gif => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
    }
}

fn image_response(bytes: Bytes, output_format: OutputFormat) -> Response {
    let content_type = match output_format {
        OutputFormat::Webp => "image/webp",
        OutputFormat::Gif => "image/gif",
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, CACHE_CONTROL)
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .body(Body::from(bytes))
        .expect("valid response headers")
}

fn env_value<T>(name: &str, default: T) -> T
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    env::var(name)
        .map(|value| {
            value
                .parse()
                .unwrap_or_else(|error| panic!("invalid {name}: {error}"))
        })
        .unwrap_or(default)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl+C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install termination handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_mp4_for_image_url() {
        assert_eq!(
            upstream_filename("HTNiiJrbAAA-Ya5.webp", "mp4"),
            "HTNiiJrbAAA-Ya5.mp4"
        );
        assert_eq!(
            upstream_filename("HTNiiJrbAAA-Ya5.gif", "mp4"),
            "HTNiiJrbAAA-Ya5.mp4"
        );
        assert_eq!(
            upstream_filename("HTNiiJrbAAA-Ya5.mp4", "mp4"),
            "HTNiiJrbAAA-Ya5.mp4"
        );
    }

    #[test]
    fn tweet_videos_drop_the_query() {
        let upstream = Url::parse(TWITTER_UPSTREAM).unwrap();

        assert_eq!(
            source_url(&upstream, &["tweet_video", "HTNiiJrbAAA-Ya5.mp4"], None)
                .unwrap()
                .as_str(),
            "https://video.twimg.com/tweet_video/HTNiiJrbAAA-Ya5.mp4"
        );
    }

    #[test]
    fn reddit_previews_keep_their_signed_query() {
        let upstream = Url::parse(REDDIT_PREVIEW_UPSTREAM).unwrap();
        let filename = upstream_filename("XBb3yF47m8XL.webp", "gif");

        assert_eq!(
            source_url(&upstream, &[&filename], Some("width=556&format=mp4&s=70a4"))
                .unwrap()
                .as_str(),
            "https://external-preview.redd.it/XBb3yF47m8XL.gif?width=556&format=mp4&s=70a4"
        );
        assert_eq!(
            source_url(&upstream, &["..", "a/b.gif"], None)
                .unwrap()
                .as_str(),
            "https://external-preview.redd.it/a%2Fb.gif"
        );
    }

    #[test]
    fn webp_response_has_webp_content_type() {
        let response = image_response(Bytes::from_static(b"RIFF\0\0\0\0WEBP"), OutputFormat::Webp);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/webp");
        assert!(is_image(b"RIFF\0\0\0\0WEBP", OutputFormat::Webp));
    }

    #[test]
    fn gif_response_has_gif_content_type() {
        let response = image_response(Bytes::from_static(b"GIF89a"), OutputFormat::Gif);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/gif");
        assert!(is_image(b"GIF89a", OutputFormat::Gif));
    }

    #[tokio::test]
    async fn limits_output_reads() {
        assert!(matches!(
            read_limited(&b"12345"[..], 4).await,
            Err(ServiceError::TooLarge)
        ));
    }
}
