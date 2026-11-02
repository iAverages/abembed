// the provider worker for hosts that block cloudflare, run on servers they don't block
use axum::{
    Router,
    extract::State,
    http::{HeaderMap, HeaderName, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use reqwest::{Client, Url, redirect::Policy};
use serde::Deserialize;
use std::{env, time::Duration};
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;

// upstream headers abembed reads, the rest are dropped
const PASSED_HEADERS: [HeaderName; 3] =
    [header::LOCATION, header::SET_COOKIE, header::CONTENT_TYPE];

#[derive(Clone)]
struct AppState {
    token: String,
    follow: Client,
    manual: Client,
}

#[derive(Deserialize)]
struct FetchRequest {
    url: String,
    #[serde(default)]
    headers: Vec<(String, String)>,
    #[serde(default)]
    follow_redirects: bool,
}

type Rejection = (StatusCode, String);

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let token = env::var("HOSTED_PROVIDER_TOKEN")
        .ok()
        .filter(|token| !token.is_empty())
        .expect("HOSTED_PROVIDER_TOKEN");
    let address = env::var("HOSTED_PROVIDER_LISTEN").unwrap_or_else(|_| "0.0.0.0:3003".into());
    let listener = TcpListener::bind(&address)
        .await
        .unwrap_or_else(|error| panic!("listen on {address}: {error}"));
    tracing::info!("listening on {address}");
    axum::serve(listener, app(token))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("serve hosted-provider");
}

fn app(token: String) -> Router {
    let client = |policy| {
        Client::builder()
            .redirect(policy)
            .timeout(Duration::from_secs(30))
            .build()
            .expect("HTTP client")
    };

    Router::new()
        .route("/health", get(|| async { StatusCode::OK }))
        .route("/fetch", post(fetch))
        .layer(TraceLayer::new_for_http())
        .with_state(AppState {
            token,
            follow: client(Policy::limited(10)),
            manual: client(Policy::none()),
        })
}

// the body is parsed after the token check so unauthorized callers learn nothing
async fn fetch(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, Rejection> {
    let authorized = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|token| same(token, &state.token));
    if !authorized {
        return Err((StatusCode::UNAUTHORIZED, "unauthorized".into()));
    }

    let request: FetchRequest = serde_json::from_slice(&body)
        .map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))?;
    let url = Url::parse(&request.url)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https"))
        .ok_or((StatusCode::BAD_REQUEST, "url must be http(s)".into()))?;

    let client = match request.follow_redirects {
        true => &state.follow,
        false => &state.manual,
    };
    let upstream = request
        .headers
        .iter()
        .fold(client.get(url), |builder, (name, value)| {
            builder.header(name, value)
        })
        .send()
        .await
        .map_err(upstream_failed)?;

    let status = upstream.status();
    let mut passed = HeaderMap::new();
    for name in PASSED_HEADERS {
        for value in upstream.headers().get_all(&name) {
            passed.append(name.clone(), value.clone());
        }
    }
    let body = upstream.bytes().await.map_err(upstream_failed)?;

    Ok((status, passed, body).into_response())
}

fn upstream_failed(error: reqwest::Error) -> Rejection {
    tracing::warn!(%error, "upstream request failed");
    (StatusCode::BAD_GATEWAY, error.to_string())
}

// constant time, the token can't be guessed byte by byte from response timings
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0, |diff, (x, y)| diff | (x ^ y))
            == 0
}

async fn shutdown_signal() {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install termination handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = terminate.recv() => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::{AppendHeaders, Redirect};
    use serde_json::{Value, json};

    const TOKEN: &str = "secret";

    async fn serve(router: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        base
    }

    // an upstream page that echoes the user agent, and a redirect to it
    async fn upstream() -> String {
        serve(
            Router::new()
                .route(
                    "/page",
                    get(|headers: HeaderMap| async move {
                        (
                            AppendHeaders([
                                (header::SET_COOKIE, "a=1"),
                                (header::SET_COOKIE, "b=2"),
                                (header::SERVER, "upstream"),
                            ]),
                            headers[header::USER_AGENT].to_str().unwrap().to_string(),
                        )
                    }),
                )
                .route("/redirect", get(|| async { Redirect::temporary("/page") })),
        )
        .await
    }

    async fn fetch(token: Option<&str>, request: Value) -> reqwest::Response {
        let provider = serve(app(TOKEN.into())).await;
        // like abembed, redirects passed back must not be followed by the caller
        let client = Client::builder().redirect(Policy::none()).build().unwrap();
        let builder = client.post(format!("{provider}/fetch"));
        match token {
            Some(token) => builder.bearer_auth(token),
            None => builder,
        }
        .body(request.to_string())
        .send()
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn rejects_missing_and_wrong_tokens() {
        let request = json!({ "url": format!("{}/page", upstream().await) });

        for token in [None, Some("wrong"), Some("secreT")] {
            let response = fetch(token, request.clone()).await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{token:?}");
        }
    }

    #[tokio::test]
    async fn rejects_non_http_urls() {
        let response = fetch(Some(TOKEN), json!({ "url": "file:///etc/passwd" })).await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn returns_redirects_when_not_following() {
        let request = json!({ "url": format!("{}/redirect", upstream().await) });
        let response = fetch(Some(TOKEN), request).await;

        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(response.headers()[header::LOCATION], "/page");
    }

    #[tokio::test]
    async fn follows_redirects_with_the_given_headers() {
        let request = json!({
            "url": format!("{}/redirect", upstream().await),
            "headers": [["User-Agent", "abembed-test"]],
            "follow_redirects": true,
        });
        let response = fetch(Some(TOKEN), request).await;

        assert_eq!(response.status(), StatusCode::OK);
        let cookies: Vec<_> = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .collect();
        assert_eq!(cookies, ["a=1", "b=2"]);
        assert!(response.headers().get(header::SERVER).is_none());
        assert_eq!(response.text().await.unwrap(), "abembed-test");
    }
}
