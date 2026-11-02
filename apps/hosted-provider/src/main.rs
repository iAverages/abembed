// the provider worker for hosts that block cloudflare, run on servers they don't block
use axum::{
    Router,
    extract::State,
    http::{HeaderMap, HeaderName, HeaderValue, Request, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper_openssl::client::legacy::HttpsConnector;
use hyper_util::{
    client::legacy::{
        Client,
        connect::{HttpConnector, dns::Name},
    },
    rt::TokioExecutor,
};
use openssl::ssl::{SslConnector, SslMethod, SslOptions};
use serde::Deserialize;
use std::{
    env, fmt, io,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
    vec,
};
use tokio::{
    net::{TcpListener, lookup_host},
    time::timeout,
};
use tower_http::trace::TraceLayer;
use tower_service::Service;
use url::{Position, Url};

const PASSED_HEADERS: [HeaderName; 3] =
    [header::LOCATION, header::SET_COOKIE, header::CONTENT_TYPE];

const SENSITIVE_HEADERS: [HeaderName; 4] = [
    header::AUTHORIZATION,
    header::COOKIE,
    header::PROXY_AUTHORIZATION,
    header::WWW_AUTHENTICATE,
];

const MAX_REDIRECTS: usize = 10;

const SSL_OP_LEGACY_EC_POINT_FORMATS: u64 = 1 << 36;

type HttpClient = Client<HttpsConnector<HttpConnector<Ipv6First>>, Empty<Bytes>>;

#[derive(Clone)]
struct Ipv6First;

impl Service<Name> for Ipv6First {
    type Response = vec::IntoIter<SocketAddr>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Self::Response>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future {
        Box::pin(async move {
            let mut addresses: Vec<_> = lookup_host((name.as_str(), 0)).await?.collect();
            addresses.sort_by_key(SocketAddr::is_ipv4);
            Ok(addresses.into_iter())
        })
    }
}

#[derive(Clone)]
struct AppState {
    token: String,
    client: HttpClient,
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
    // outgoing connections only leave through this interface, to send them over a tunnel
    let interface = env::var("HOSTED_PROVIDER_INTERFACE").ok();
    let address = env::var("HOSTED_PROVIDER_LISTEN").unwrap_or_else(|_| "0.0.0.0:3003".into());
    let listener = TcpListener::bind(&address)
        .await
        .unwrap_or_else(|error| panic!("listen on {address}: {error}"));
    tracing::info!("listening on {address}");
    axum::serve(listener, app(token, interface))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("serve hosted-provider");
}

fn app(token: String, interface: Option<String>) -> Router {
    Router::new()
        .route("/health", get(|| async { StatusCode::OK }))
        .route("/fetch", post(fetch))
        .layer(TraceLayer::new_for_http())
        .with_state(AppState {
            token,
            client: Client::builder(TokioExecutor::new())
                .http1_title_case_headers(true)
                .build(connector(interface)),
        })
}

// matches the client hello `curl --http1.1` sends with the same openssl, hosts like reddit block
// anything else. http/1.1 only because hyper's h2 settings frame can't be made to look like curl's
fn connector(interface: Option<String>) -> HttpsConnector<HttpConnector<Ipv6First>> {
    let mut http = HttpConnector::new_with_resolver(Ipv6First);
    if let Some(interface) = interface {
        http.set_interface(interface);
    }
    http.enforce_http(false);

    let mut tls = SslConnector::builder(SslMethod::tls()).expect("TLS connector");
    tls.set_alpn_protos(b"\x08http/1.1").expect("ALPN");
    tls.set_options(
        SslOptions::NO_TICKET | SslOptions::from_bits_retain(SSL_OP_LEGACY_EC_POINT_FORMATS),
    );
    // SAFETY: the context pointer is valid for as long as the builder lives
    unsafe { openssl_sys::SSL_CTX_set_post_handshake_auth(tls.as_ptr(), 1) };

    HttpsConnector::with_connector(http, tls).expect("HTTPS connector")
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

    let mut upstream_headers = HeaderMap::new();
    for (name, value) in request.headers {
        let name = HeaderName::try_from(name)
            .map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))?;
        let value = HeaderValue::try_from(value)
            .map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))?;
        upstream_headers.append(name, value);
    }
    upstream_headers
        .entry(header::ACCEPT)
        .or_insert(HeaderValue::from_static("*/*"));

    let (status, headers, body) = timeout(
        Duration::from_secs(30),
        send(
            &state.client,
            url.clone(),
            upstream_headers,
            request.follow_redirects,
        ),
    )
    .await
    .map_err(upstream_failed)??;
    tracing::info!(%url, %status, "fetched");

    let mut passed = HeaderMap::new();
    for name in PASSED_HEADERS {
        for value in headers.get_all(&name) {
            passed.append(name.clone(), value.clone());
        }
    }

    Ok((status, passed, body).into_response())
}

async fn send(
    client: &HttpClient,
    mut url: Url,
    mut headers: HeaderMap,
    follow_redirects: bool,
) -> Result<(StatusCode, HeaderMap, Bytes), Rejection> {
    for _ in 0..=MAX_REDIRECTS {
        let mut request = Request::get(url.as_str())
            .body(Empty::new())
            .map_err(upstream_failed)?;
        // host first like curl, hyper appends it after the rest
        let host = &url[Position::BeforeHost..Position::AfterPort];
        request.headers_mut().insert(
            header::HOST,
            HeaderValue::try_from(host).map_err(upstream_failed)?,
        );
        request.headers_mut().extend(headers.clone());
        let response = client.request(request).await.map_err(upstream_failed)?;

        let redirect = match response.status() {
            StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT => response.headers().get(header::LOCATION),
            _ => None,
        };
        if let Some(location) = redirect.filter(|_| follow_redirects) {
            let next = location
                .to_str()
                .map_err(upstream_failed)
                .and_then(|location| url.join(location).map_err(upstream_failed))?;
            if next.host_str() != url.host_str()
                || next.port_or_known_default() != url.port_or_known_default()
            {
                for name in &SENSITIVE_HEADERS {
                    headers.remove(name);
                }
            }
            url = next;
            continue;
        }

        let (parts, body) = response.into_parts();
        let body = body.collect().await.map_err(upstream_failed)?.to_bytes();
        return Ok((parts.status, parts.headers, body));
    }
    Err(upstream_failed("too many redirects"))
}

fn upstream_failed(error: impl fmt::Debug + fmt::Display) -> Rejection {
    tracing::warn!(?error, "upstream request failed");
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
    use reqwest::{Client, redirect::Policy};
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
        let provider = serve(app(TOKEN.into(), None)).await;
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
    async fn fails_instead_of_bypassing_a_missing_interface() {
        let provider = serve(app(TOKEN.into(), Some("missing0".into()))).await;
        let request = json!({ "url": format!("{}/page", upstream().await) });
        let response = Client::new()
            .post(format!("{provider}/fetch"))
            .bearer_auth(TOKEN)
            .body(request.to_string())
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
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
