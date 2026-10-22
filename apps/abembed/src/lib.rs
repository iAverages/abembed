mod accounts;
mod admin;
mod sources;

use axum::{
    http::{header, uri::PathAndQuery, Uri},
    response::{IntoResponse, Redirect},
    routing::get,
    Router,
};
use sources::{
    facebook::{self, FacebookSource},
    header_str,
    instagram::{self, InstagramSource},
    is_platform_host,
    tiktok::{self, TikTokSource},
    Source,
};
use tower_http::normalize_path::NormalizePath;
use tower_service::Service;
use worker::*;

const PLATFORM_HOSTS: [(&[&str], &str); 3] = [
    (InstagramSource::HOSTS, InstagramSource::PLATFORM),
    (FacebookSource::HOSTS, FacebookSource::PLATFORM),
    (TikTokSource::HOSTS, TikTokSource::PLATFORM),
];

// routes that are the same on every domain
const UNPREFIXED_ROUTES: [&str; 3] = ["api", "admin", "openapi.json"];

const REPOSITORY_URL: &str = "https://github.com/iaverages/abembed";

fn router(env: Env) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/openapi.json", get(openapi))
        .merge(facebook::get_router())
        .merge(instagram::get_router())
        .merge(tiktok::get_router())
        .merge(admin::get_router())
        .with_state(env)
}

#[event(fetch)]
async fn fetch(
    req: HttpRequest,
    env: Env,
    _ctx: Context,
) -> Result<axum::http::Response<axum::body::Body>> {
    Ok(NormalizePath::trim_trailing_slash(router(env))
        .call(rewrite_platform_host(req))
        .await?)
}

// handle rerouting domains interanlly
fn rewrite_platform_host(mut req: HttpRequest) -> HttpRequest {
    let host = header_str(req.headers(), header::HOST);
    let Some(path) = platform_path(host, req.uri().path()) else {
        return req;
    };
    let path_and_query = match req.uri().query() {
        Some(query) => format!("{path}?{query}"),
        None => path.to_string(),
    };
    let Ok(path_and_query) = path_and_query.parse::<PathAndQuery>() else {
        return req;
    };

    let mut parts = req.uri().clone().into_parts();
    parts.path_and_query = Some(path_and_query);
    if let Ok(uri) = Uri::from_parts(parts) {
        *req.uri_mut() = uri;
    }
    req
}

fn platform_path(host_header: &str, path: &str) -> Option<String> {
    let (_, platform) = PLATFORM_HOSTS
        .iter()
        .find(|(hosts, _)| is_platform_host(hosts, host_header))?;

    let first = path
        .trim_start_matches('/')
        .split('/')
        .next()
        .unwrap_or_default();
    if first == *platform || UNPREFIXED_ROUTES.contains(&first) {
        return None;
    }
    Some(format!("/{platform}{path}"))
}

pub async fn root() -> Redirect {
    Redirect::temporary(REPOSITORY_URL)
}

// temp solution
async fn openapi() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/json")],
        include_str!("openapi.json"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_hosts_get_their_prefix() {
        for (hosts, platform) in PLATFORM_HOSTS {
            for host in hosts {
                assert_eq!(
                    platform_path(host, "/p/AbC_123/").as_deref(),
                    Some(format!("/{platform}/p/AbC_123/").as_str())
                );
                assert_eq!(
                    platform_path(&format!("staging.{host}"), "/reel/123").as_deref(),
                    Some(format!("/{platform}/reel/123").as_str())
                );
            }
        }
    }

    #[test]
    fn rewritten_requests_keep_their_origin_and_query() {
        let req = axum::http::Request::builder()
            .uri("https://abinstagram.com/p/AbC_123/?igsh=1")
            .header(header::HOST, "abinstagram.com")
            .body(worker::Body::empty())
            .unwrap();

        assert_eq!(
            rewrite_platform_host(req).uri(),
            "https://abinstagram.com/instagram/p/AbC_123/?igsh=1"
        );
    }

    #[test]
    fn prefixed_and_shared_routes_are_left_alone() {
        assert!(platform_path("abinstagram.com", "/instagram/p/AbC_123").is_none());
        assert!(platform_path("abinstagram.com", "/api/instagram/p/AbC_123").is_none());
        assert!(platform_path("abinstagram.com", "/openapi.json").is_none());
        assert!(platform_path("abembed.com", "/p/AbC_123").is_none());
        assert!(platform_path("notabinstagram.com", "/p/AbC_123").is_none());
    }

    #[test]
    fn openapi_is_valid_json() {
        serde_json::from_str::<serde_json::Value>(include_str!("openapi.json")).unwrap();
    }
}
