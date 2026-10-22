pub mod facebook;
pub mod html;
pub mod instagram;
pub mod tiktok;

use axum::{
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use html::{discord_image_post_html, Site};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::{cell::Cell, iter};
use worker::{js_sys, web_sys, Env, Fetcher, Headers, KvStore, RequestInit, RequestRedirect, Url};

#[derive(Debug, PartialEq, Eq)]
pub struct HttpError(pub StatusCode, pub String);

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        json_response(self.0, &json!({ "error": self.1 }))
    }
}

const CACHE_BINDING: &str = "POST_CACHE";
const DISABLE_BROWSER_REDIRECT_VAR: &str = "DISABLE_BROWSER_REDIRECT";
const CACHE_TTL_SECONDS: u64 = 60 * 60;

pub fn json_response(status: StatusCode, body: &Value) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body.to_string(),
    )
        .into_response()
}

pub fn bad_gateway(message: impl Into<String>) -> HttpError {
    HttpError(StatusCode::BAD_GATEWAY, message.into())
}

pub fn bad_request(message: impl Into<String>) -> HttpError {
    HttpError(StatusCode::BAD_REQUEST, message.into())
}

pub fn internal(message: impl Into<String>) -> HttpError {
    HttpError(StatusCode::INTERNAL_SERVER_ERROR, message.into())
}

pub fn header_str(headers: &HeaderMap, name: header::HeaderName) -> &str {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

// crawlers like discordbot also claim `Mozilla/` and accept html
pub fn is_browser_navigation(headers: &HeaderMap) -> bool {
    headers
        .get("sec-fetch-mode")
        .is_some_and(|mode| mode == "navigate")
}

pub fn redirects_browser(env: &Env, headers: &HeaderMap) -> bool {
    is_browser_navigation(headers)
        && !env
            .var(DISABLE_BROWSER_REDIRECT_VAR)
            .is_ok_and(|value| value.to_string() == "true")
}

pub trait Source {
    const PLATFORM: &'static str;
    const SITE: Site;

    const HOSTS: &'static [&'static str];
    const FORCE_FULL_EMBED: bool = false;

    fn id(&self) -> &str;
    fn cache_key(&self) -> String;
    fn source_url(&self) -> String;

    fn redirect_url(&self) -> String {
        self.source_url()
    }

    fn media_url(&self, base: &str, index: usize) -> String;
    fn avatar_url(&self, base: &str) -> String;
    async fn fetch(&self, env: &Env, providers: &Providers) -> Result<EmbedPost, HttpError>;

    fn embed_html(&self, post: &EmbedPost, base: &str) -> String {
        gallery_html(self, post, base)
    }
}

pub fn gallery_html<S: Source + ?Sized>(source: &S, post: &EmbedPost, base: &str) -> String {
    discord_image_post_html(
        &S::SITE,
        post,
        |index| source.media_url(base, index),
        &source.avatar_url(base),
    )
}

pub async fn embed<S: Source>(
    env: &Env,
    source: &S,
    headers: &HeaderMap,
) -> Result<Response, HttpError> {
    if redirects_browser(env, headers) {
        return Ok(redirect(&source.redirect_url()));
    }

    let post = load_post(env, source).await?;
    let base = media_base(S::HOSTS, S::PLATFORM, headers);
    Ok(embed_response(&post, S::FORCE_FULL_EMBED, || {
        source.embed_html(&post, &base)
    }))
}

pub async fn media(env: &Env, source: &impl Source, index: usize) -> Result<Response, HttpError> {
    let post = load_post(env, source).await?;
    let media = post.media.get(index).ok_or(HttpError(
        StatusCode::NOT_FOUND,
        "Media not found".to_string(),
    ))?;

    Ok(redirect(&media.url))
}

pub async fn avatar(env: &Env, source: &impl Source) -> Result<Response, HttpError> {
    let post = load_post(env, source).await?;
    let avatar_url = post
        .author
        .as_ref()
        .and_then(|author| author.avatar_url.as_deref())
        .ok_or(HttpError(
            StatusCode::NOT_FOUND,
            "Avatar not found".to_string(),
        ))?;

    Ok(redirect(avatar_url))
}

pub async fn api<S: Source>(
    env: &Env,
    source: &S,
    headers: &HeaderMap,
) -> Result<Response, HttpError> {
    let post = load_post(env, source).await?;
    if let Some(error) = gated_error(&post) {
        return Err(error);
    }
    let base = media_base(S::HOSTS, S::PLATFORM, headers);
    let post = post_json(
        &post,
        |index| source.media_url(&base, index),
        &source.avatar_url(&base),
    );
    Ok(json_response(StatusCode::OK, &post))
}

pub fn gated_error(post: &EmbedPost) -> Option<HttpError> {
    post.media.is_empty().then(|| {
        HttpError(
            StatusCode::FORBIDDEN,
            post.title
                .clone()
                .unwrap_or_else(|| "Post is gated".to_string()),
        )
    })
}

// single media posts link straight to the media unless the embed is forced
pub fn embed_response(post: &EmbedPost, force: bool, render: impl FnOnce() -> String) -> Response {
    match post.media.as_slice() {
        [media] if !force => redirect(&media.url),
        _ => html::html_response(render()),
    }
}

pub fn cdn_url(url: &str, domains: &[&str]) -> Option<String> {
    let url = Url::parse(url).ok()?;
    let host = url.host_str()?;
    let allowed = domains
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")));

    (url.scheme() == "https" && allowed).then(|| url.to_string())
}

pub fn redirect(location: &str) -> Response {
    (
        StatusCode::FOUND,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::LOCATION, location),
        ],
    )
        .into_response()
}

pub fn is_platform_host(hosts: &[&str], host_header: &str) -> bool {
    let host = host_header.split(':').next().unwrap_or_default();
    hosts
        .iter()
        .any(|known| host == *known || host.ends_with(&format!(".{known}")))
}

// media links point back at whichever domain served the embed
pub fn media_base(hosts: &[&str], platform: &str, headers: &HeaderMap) -> String {
    let host = header_str(headers, header::HOST);
    if is_platform_host(hosts, host) {
        format!("https://{host}")
    } else {
        format!("https://{host}/{platform}")
    }
}

pub fn post_cache(env: &Env) -> Result<KvStore, HttpError> {
    env.kv(CACHE_BINDING)
        .map_err(|error| internal(error.to_string()))
}

pub async fn get_cached<T: DeserializeOwned>(cache: &KvStore, key: &str) -> Option<T> {
    cache.get(key).json::<T>().await.unwrap_or_else(|error| {
        warn(
            &json!({ "event": "post_cache", "op": "get", "key": key, "error": error.to_string() }),
        );
        None
    })
}

pub async fn put_cached(cache: &KvStore, key: &str, value: &impl Serialize) {
    let put = async {
        cache
            .put(key, value)?
            .expiration_ttl(CACHE_TTL_SECONDS)
            .execute()
            .await
    };
    if let Err(error) = put.await {
        warn(
            &json!({ "event": "post_cache", "op": "put", "key": key, "error": error.to_string() }),
        );
    }
}

pub fn objects(root: &Value) -> impl Iterator<Item = &Map<String, Value>> {
    let mut stack = vec![root];
    iter::from_fn(move || {
        while let Some(value) = stack.pop() {
            match value {
                Value::Object(map) => {
                    stack.extend(map.values().rev());
                    return Some(map);
                }
                Value::Array(items) => stack.extend(items.iter().rev()),
                _ => {}
            }
        }
        None
    })
}

pub fn log(fields: &Value) {
    if let Ok(object) = js_sys::JSON::parse(&fields.to_string()) {
        web_sys::console::log_1(&object);
    }
}

pub fn warn(fields: &Value) {
    if let Ok(object) = js_sys::JSON::parse(&fields.to_string()) {
        web_sys::console::warn_1(&object);
    }
}

pub async fn load_post<S: Source>(env: &Env, source: &S) -> Result<EmbedPost, HttpError> {
    let cache = post_cache(env)?;
    let key = source.cache_key();

    if let Some(post) = get_cached(&cache, &key).await {
        log(&json!({
            "event": "embed",
            "platform": S::PLATFORM,
            "id": source.id(),
            "cache": "hit",
        }));
        return Ok(post);
    }

    let providers = Providers::from_env(env)?;
    let result = source.fetch(env, &providers).await;
    log(&json!({
        "event": "embed",
        "platform": S::PLATFORM,
        "id": source.id(),
        "cache": "miss",
        "provider": providers.used(),
        "media": result.as_ref().ok().map(|post| post.media.len()),
        "error": result.as_ref().err().map(|HttpError(_, message)| message),
    }));
    let post = result?;
    put_cached(&cache, &key, &post).await;

    Ok(post)
}

pub fn author_name(author: &Author) -> String {
    match &author.display_name {
        Some(name) if author.username.is_empty() => name.clone(),
        Some(name) if *name != author.username => format!("{name} (@{})", author.username),
        _ => format!("@{}", author.username),
    }
}

pub fn post_json(
    embed: &EmbedPost,
    media_url: impl Fn(usize) -> String,
    avatar_url: &str,
) -> Value {
    let stats = embed.stats.clone().unwrap_or_default();
    let media: Vec<Value> = embed
        .media
        .iter()
        .enumerate()
        .map(|(index, media)| {
            json!({
                "type": media.kind,
                "url": media_url(index),
                "width": media.width,
                "height": media.height,
            })
        })
        .collect();

    json!({
        "platform": embed.provider,
        "id": embed.provider_id,
        "url": embed.source_url,
        "description": embed.description,
        "author": embed.author.as_ref().map(|author| json!({
            "name": author_name(author),
            "username": author.username,
            "url": author.profile_url,
            "avatar_url": author.avatar_url.as_ref().map(|_| avatar_url),
        })),
        "stats": {
            "likes": stats.likes,
            "reposts": stats.shares,
            "comments": stats.comments,
        },
        "media": media,
    })
}

pub const REGIONS: [&str; 4] = ["a", "b", "c", "d"];

pub struct ProviderResponse {
    pub status: u16,
    pub location: Option<String>,
    pub set_cookies: Vec<String>,
    pub body: String,
}

// providers are region locked, helps to avoid region restirctions and keep accounts to appear from
// the same region
pub struct Providers {
    fetchers: Vec<Fetcher>,
    // last region that answered, used for logging
    used: Cell<Option<&'static str>>,
}

impl Providers {
    pub fn from_env(env: &Env) -> Result<Self, HttpError> {
        REGIONS
            .iter()
            .map(|region| env.service(&format!("PROVIDER_{}", region.to_ascii_uppercase())))
            .collect::<worker::Result<_>>()
            .map(|fetchers| Self {
                fetchers,
                used: Cell::new(None),
            })
            .map_err(|error| internal(error.to_string()))
    }

    pub fn used(&self) -> Option<&'static str> {
        self.used.get()
    }

    pub async fn fetch<T>(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        parse: impl Fn(&str) -> Result<T, HttpError>,
    ) -> Result<T, HttpError> {
        for region in REGIONS.iter().cycle().take(REGIONS.len() * 2) {
            let attempt = async {
                let response = self
                    .fetch_raw(region, url, headers, RequestRedirect::Follow)
                    .await?;
                if !(200..300).contains(&response.status) {
                    return Err(bad_gateway(format!("{url} returned {}", response.status)));
                }
                parse(&response.body)
            };
            match attempt.await {
                Ok(value) => return Ok(value),
                Err(HttpError(_, message)) => warn(
                    &json!({ "event": "provider", "region": region, "url": url, "error": message }),
                ),
            }
        }

        Err(bad_gateway("Regional provider workers failed"))
    }

    pub async fn fetch_redirect<T>(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        parse: impl Fn(&str) -> Option<T>,
    ) -> Result<T, HttpError> {
        for region in REGIONS {
            match self
                .fetch_raw(region, url, headers, RequestRedirect::Manual)
                .await
            {
                Ok(response) => {
                    let target = response
                        .location
                        .filter(|_| (300..400).contains(&response.status))
                        .and_then(|location| Url::parse(url).ok()?.join(&location).ok())
                        .and_then(|location| parse(location.as_str()));
                    match target {
                        Some(value) => return Ok(value),
                        None => warn(&json!({
                            "event": "provider",
                            "region": region,
                            "url": url,
                            "status": response.status,
                            "error": "no usable redirect",
                        })),
                    }
                }
                Err(HttpError(_, message)) => warn(
                    &json!({ "event": "provider", "region": region, "url": url, "error": message }),
                ),
            }
        }

        Err(bad_gateway("Regional provider workers failed"))
    }

    pub async fn fetch_raw(
        &self,
        region: &str,
        url: &str,
        headers: &[(&str, &str)],
        redirect: RequestRedirect,
    ) -> Result<ProviderResponse, HttpError> {
        let index = REGIONS
            .iter()
            .position(|known| *known == region)
            .ok_or_else(|| internal(format!("Unknown provider region {region}")))?;

        let provider = &self.fetchers[index];
        let failed = |error: worker::Error| bad_gateway(format!("Failed to fetch {url}: {error}"));

        let mut init = RequestInit::new();
        init.with_headers(headers.iter().collect::<Headers>())
            .with_redirect(redirect);
        let response = provider.fetch(url, Some(init)).await.map_err(failed)?;

        let status = response.status().as_u16();

        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);

        let set_cookies = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(str::to_string)
            .collect();

        let body = worker::Response::try_from(response)
            .map_err(failed)?
            .text()
            .await
            .map_err(failed)?;

        self.used.set(Some(REGIONS[index]));
        Ok(ProviderResponse {
            status,
            location,
            set_cookies,
            body,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedPost {
    pub provider: String,
    pub provider_id: String,

    pub source_url: String,

    pub title: Option<String>,
    pub description: Option<String>,
    pub author: Option<Author>,

    pub media: Vec<Media>,
    pub stats: Option<Stats>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Author {
    pub id: Option<String>,
    pub username: String,
    pub display_name: Option<String>,
    pub profile_url: Option<String>,
    pub avatar_url: Option<String>,
    pub verified: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Media {
    pub kind: MediaKind,
    pub url: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Video,
    Image,
    Audio,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Stats {
    pub views: Option<u64>,
    pub likes: Option<u64>,
    pub comments: Option<u64>,
    pub shares: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(name, value)| (name.parse().unwrap(), value.parse().unwrap()))
            .collect()
    }

    #[test]
    fn browser_navigation_is_detected() {
        assert!(is_browser_navigation(&headers(&[
            ("user-agent", "Mozilla/5.0 Chrome/136.0.0.0"),
            ("accept", "text/html"),
            ("sec-fetch-mode", "navigate"),
        ])));
    }

    fn post(media: &[&str]) -> EmbedPost {
        EmbedPost {
            provider: "test".to_string(),
            provider_id: "1".to_string(),
            source_url: String::new(),
            title: None,
            description: Some("text".to_string()),
            author: None,
            media: media
                .iter()
                .map(|url| Media {
                    kind: MediaKind::Image,
                    url: url.to_string(),
                    width: None,
                    height: None,
                })
                .collect(),
            stats: None,
        }
    }

    fn host(value: &str) -> HeaderMap {
        HeaderMap::from_iter([(header::HOST, value.parse().unwrap())])
    }

    #[test]
    fn platform_domains_imply_the_prefix() {
        let hosts = &["abinstagram.com"];

        assert_eq!(
            media_base(hosts, "instagram", &host("abinstagram.com")),
            "https://abinstagram.com"
        );
        assert_eq!(
            media_base(hosts, "instagram", &host("abembed.com")),
            "https://abembed.com/instagram"
        );
    }

    #[test]
    fn single_media_posts_redirect_to_the_media() {
        let response = embed_response(
            &post(&["https://cdn.example.com/one.jpg"]),
            false,
            || unreachable!(),
        );

        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(
            response.headers()[header::LOCATION],
            "https://cdn.example.com/one.jpg"
        );
    }

    #[test]
    fn multi_media_posts_get_the_full_embed() {
        let post = post(&[
            "https://cdn.example.com/one.jpg",
            "https://cdn.example.com/two.jpg",
        ]);
        let response = embed_response(&post, false, || "<html>".to_string());

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
    }

    #[test]
    fn forced_embeds_never_redirect() {
        let post = post(&["https://cdn.example.com/one.jpg"]);
        let response = embed_response(&post, true, || "<html>".to_string());

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn discordbot_is_not_a_browser_navigation() {
        assert!(!is_browser_navigation(&headers(&[
            (
                "user-agent",
                "Mozilla/5.0 (compatible; Discordbot/2.0; +https://discordapp.com)"
            ),
            ("accept", "text/html"),
        ])));
    }
}
