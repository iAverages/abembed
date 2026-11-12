pub mod facebook;
pub mod html;
pub mod instagram;
pub mod reddit;
pub mod tiktok;
pub mod twitter;

use axum::{
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use html::{discord_component_json, discord_image_post_html, Site};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::{cell::Cell, iter};
use worker::{
    js_sys, web_sys, Env, Fetch, Fetcher, Headers, HttpResponse, KvStore, Method, RequestInit,
    RequestRedirect, Url,
};

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

pub fn sized(value: &str, len: std::ops::RangeInclusive<usize>, allowed: fn(u8) -> bool) -> bool {
    len.contains(&value.len()) && value.bytes().all(allowed)
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
    // an empty media list means the post was behind a login wall
    const GATED_WHEN_EMPTY: bool = false;

    fn id(&self) -> &str;
    fn cache_key(&self) -> String;
    fn source_url(&self) -> String;

    fn redirect_url(&self) -> String {
        self.source_url()
    }

    fn media_url(&self, base: &str, media: &str) -> String;
    fn avatar_url(&self, base: &str) -> String;

    // avatars of the posts shown under this one, by their place in `EmbedPost::entries`
    fn entry_avatar_url(&self, base: &str, index: usize) -> String {
        if index == 0 {
            self.avatar_url(base)
        } else {
            format!("{}/{index}", self.avatar_url(base))
        }
    }

    async fn fetch(&self, env: &Env, providers: &Providers) -> Result<EmbedPost, HttpError>;

    fn embed_html(&self, post: &EmbedPost, base: &str) -> String {
        gallery_html(self, post, base)
    }
}

pub fn gallery_html<S: Source + ?Sized>(source: &S, post: &EmbedPost, base: &str) -> String {
    discord_image_post_html(
        &S::SITE,
        post,
        |index| source.media_url(base, &media_file(post, index)),
        |index| source.entry_avatar_url(base, index),
    )
}

pub fn media_file(post: &EmbedPost, index: usize) -> String {
    let extension = post.all_media().nth(index).and_then(|media| {
        let url = Url::parse(&media.url).ok()?;
        [".gif", ".webp"]
            .into_iter()
            .find(|extension| url.path().ends_with(extension))
    });
    format!("{index}{}", extension.unwrap_or_default())
}

pub fn media_index(file: &str) -> Option<usize> {
    file.split('.').next()?.parse().ok()
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

pub async fn media(env: &Env, source: &impl Source, file: &str) -> Result<Response, HttpError> {
    let not_found = || HttpError(StatusCode::NOT_FOUND, "Media not found".to_string());
    let index = media_index(file).ok_or_else(not_found)?;
    let post = load_post(env, source).await?;
    let media = post.all_media().nth(index).ok_or_else(not_found)?;

    Ok(redirect(&media.url))
}

pub async fn avatar(env: &Env, source: &impl Source, index: usize) -> Result<Response, HttpError> {
    let post = load_post(env, source).await?;
    let avatar_url = post
        .entries()
        .nth(index)
        .and_then(|(_, entry)| entry.author.as_ref())
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
    if let Some(error) = gated_error(&post).filter(|_| S::GATED_WHEN_EMPTY) {
        return Err(error);
    }
    let base = media_base(S::HOSTS, S::PLATFORM, headers);
    let post = post_json(
        &post,
        |index| source.media_url(&base, &media_file(&post, index)),
        &source.avatar_url(&base),
    );
    Ok(json_response(StatusCode::OK, &post))
}

pub async fn component<S: Source>(
    env: &Env,
    source: &S,
    headers: &HeaderMap,
) -> Result<Response, HttpError> {
    let post = load_post(env, source).await?;
    if let Some(error) = gated_error(&post).filter(|_| S::GATED_WHEN_EMPTY) {
        return Err(error);
    }
    let base = media_base(S::HOSTS, S::PLATFORM, headers);
    let component = discord_component_json(
        &S::SITE,
        &post,
        |index| source.media_url(&base, &media_file(&post, index)),
        |index| source.entry_avatar_url(&base, index),
    );
    Ok((
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        component,
    )
        .into_response())
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

// animated gifs come as mp4, the gif service (apps/gif-service) serves them as webp or gif
pub const GIF_SERVICE_URL: &str = "https://gif.abembed.com";

// merges the separate dash video and audio streams, see apps/proxy
const MERGE_PROXY_URL: &str = "https://proxy.abembed.com/merge";

pub fn merged_video_url(video: &str, audio: &str) -> String {
    Url::parse_with_params(MERGE_PROXY_URL, [("video", video), ("audio", audio)])
        .unwrap()
        .to_string()
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
            // put stores strings raw, get_cached always parses json
            .put(key, serde_json::to_string(value)?)?
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
    if let Ok(object) = js_sys::JSON::parse(&with_message(fields).to_string()) {
        web_sys::console::log_1(&object);
    }
}

pub fn warn(fields: &Value) {
    if let Ok(object) = js_sys::JSON::parse(&with_message(fields).to_string()) {
        web_sys::console::warn_1(&object);
    }
}

fn with_message(fields: &Value) -> Value {
    let mut fields = fields.clone();
    let error = fields
        .as_object_mut()
        .and_then(|fields| fields.remove("error"))
        .filter(|error| !error.is_null());
    fields["message"] = error.unwrap_or_else(|| fields["event"].clone());
    fields
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

    let providers = Providers::for_platform(env, S::PLATFORM)?;
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
        "description": embed.description.as_deref().map(html::plain_description),
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

const HOSTED_PROVIDER_PLATFORMS_VAR: &str = "HOSTED_PROVIDER_PLATFORMS";
const HOSTED_PROVIDERS_SECRET: &str = "HOSTED_PROVIDERS";
const HOSTED_PROVIDER_TOKEN_SECRET: &str = "HOSTED_PROVIDER_TOKEN";
const DEBUG_PROVIDERS_VAR: &str = "DEBUG_PROVIDERS";

enum Provider {
    // apps/provider, the default fetcher for sources
    Worker(Fetcher),
    // apps/hosted-provider, for sites that block cloudflare
    Hosted { url: String, token: String },
}

// providers are region locked, helps to avoid region restirctions and keep accounts to appear from
// the same region
pub struct Providers {
    // named by region, hosted ones by url
    providers: Vec<(String, Provider)>,
    // last provider that answered, used for logging
    used: Cell<Option<usize>>,
    // log the start of failed responses
    debug: bool,
}

impl Providers {
    fn new(env: &Env, providers: Vec<(String, Provider)>) -> Self {
        Self {
            providers,
            used: Cell::new(None),
            debug: env
                .var(DEBUG_PROVIDERS_VAR)
                .is_ok_and(|value| value.to_string() == "true"),
        }
    }

    pub fn from_env(env: &Env) -> Result<Self, HttpError> {
        REGIONS
            .iter()
            .map(|region| {
                env.service(&format!("PROVIDER_{}", region.to_ascii_uppercase()))
                    .map(|fetcher| (region.to_string(), Provider::Worker(fetcher)))
            })
            .collect::<worker::Result<_>>()
            .map(|providers| Self::new(env, providers))
            .map_err(|error| internal(error.to_string()))
    }

    pub fn hosted(env: &Env) -> Result<Self, HttpError> {
        let secret = |name| {
            env.secret(name)
                .map(|secret| secret.to_string())
                .map_err(|error| internal(format!("{name}: {error}")))
        };
        let token = secret(HOSTED_PROVIDER_TOKEN_SECRET)?;
        let providers: Vec<_> = hosted_list(&secret(HOSTED_PROVIDERS_SECRET)?)
            .map(|url| {
                let provider = Provider::Hosted {
                    url: url.to_string(),
                    token: token.clone(),
                };
                (url.to_string(), provider)
            })
            .collect();
        if providers.is_empty() {
            return Err(internal(format!("{HOSTED_PROVIDERS_SECRET} has no urls")));
        }

        Ok(Self::new(env, providers))
    }

    // sites that block cloudflare go through apps/hosted-provider instead
    pub fn for_platform(env: &Env, platform: &str) -> Result<Self, HttpError> {
        let hosted = env
            .var(HOSTED_PROVIDER_PLATFORMS_VAR)
            .is_ok_and(|platforms| hosted_list(&platforms.to_string()).any(|p| p == platform));
        match hosted {
            true => Self::hosted(env),
            false => Self::from_env(env),
        }
    }

    pub fn used(&self) -> Option<&str> {
        self.used
            .get()
            .map(|index| self.providers[index].0.as_str())
    }

    pub async fn fetch<T>(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        parse: impl Fn(&str) -> Result<T, HttpError>,
    ) -> Result<T, HttpError> {
        let names = self.providers.iter().map(|(name, _)| name.as_str());
        for region in names.cycle().take(self.providers.len() * 2) {
            let result = self
                .fetch_raw(region, url, headers, RequestRedirect::Follow, None)
                .await
                .and_then(|response| {
                    let parsed = match (200..300).contains(&response.status) {
                        true => parse(&response.body),
                        false => Err(bad_gateway(format!("{url} returned {}", response.status))),
                    };
                    parsed.map_err(|HttpError(status, message)| match self.debug {
                        true => HttpError(
                            status,
                            format!("{message}, body: {}", excerpt(&response.body)),
                        ),
                        false => HttpError(status, message),
                    })
                });
            match result {
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
        for (region, _) in &self.providers {
            match self
                .fetch_raw(region, url, headers, RequestRedirect::Manual, None)
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
                            "body": self.debug.then(|| excerpt(&response.body)),
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
        body: Option<&str>,
    ) -> Result<ProviderResponse, HttpError> {
        let index = self
            .providers
            .iter()
            .position(|(name, _)| name == region)
            .ok_or_else(|| internal(format!("Unknown provider region {region}")))?;

        let failed = |error: worker::Error| bad_gateway(format!("Failed to fetch {url}: {error}"));

        let response = match &self.providers[index].1 {
            Provider::Worker(fetcher) => {
                let mut init = RequestInit::new();
                init.with_headers(headers.iter().collect::<Headers>())
                    .with_redirect(redirect);
                if let Some(body) = body {
                    init.with_method(Method::Post).with_body(Some(body.into()));
                }
                fetcher.fetch(url, Some(init)).await
            }
            Provider::Hosted { .. } if body.is_some() => {
                return Err(internal("Hosted providers cannot send a request body"));
            }
            Provider::Hosted { url: base, token } => {
                let body = json!({
                    "url": url,
                    "headers": headers,
                    "follow_redirects": matches!(redirect, RequestRedirect::Follow),
                });
                let authorization = format!("Bearer {token}");
                let mut init = RequestInit::new();
                init.with_method(Method::Post)
                    .with_headers(
                        [
                            ("Authorization", authorization.as_str()),
                            ("Content-Type", "application/json"),
                        ]
                        .iter()
                        .collect::<Headers>(),
                    )
                    .with_body(Some(body.to_string().into()))
                    .with_redirect(RequestRedirect::Manual);
                let request = worker::Request::new_with_init(&format!("{base}/fetch"), &init);
                match request {
                    Ok(request) => Fetch::Request(request)
                        .send()
                        .await
                        .and_then(HttpResponse::try_from),
                    Err(error) => Err(error),
                }
            }
        }
        .map_err(failed)?;

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

        self.used.set(Some(index));
        Ok(ProviderResponse {
            status,
            location,
            set_cookies,
            body,
        })
    }
}

fn excerpt(body: &str) -> String {
    let collapsed = body.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(1000).collect()
}

fn hosted_list(list: &str) -> impl Iterator<Item = &str> {
    list.split(',')
        .map(|url| url.trim().trim_end_matches('/'))
        .filter(|url| !url.is_empty())
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

    #[serde(default)]
    pub lang: Option<String>,

    #[serde(default)]
    pub reply_to: Option<Box<EmbedPost>>,
    #[serde(default)]
    pub quote: Option<Box<EmbedPost>>,
    #[serde(default)]
    pub translation: Option<Translation>,
}

impl EmbedPost {
    pub fn entries(&self) -> impl Iterator<Item = (Option<&'static str>, &EmbedPost)> {
        iter::once((None, self))
            .chain(
                self.reply_to
                    .as_deref()
                    .map(|post| (Some("Replying to"), post)),
            )
            .chain(
                self.quote
                    .as_deref()
                    .map(|post| (Some("Quoted post"), post)),
            )
    }

    pub fn all_media(&self) -> impl Iterator<Item = &Media> {
        self.entries().flat_map(|(_, post)| post.media.iter())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Translation {
    pub text: String,
    pub source_lang: String,
    pub target_lang: String,
    pub provider: String,
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
    pub alt: Option<String>,
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

    #[test]
    fn hosted_provider_urls_are_comma_separated() {
        let urls: Vec<_> =
            hosted_list(" https://a.example.com/, https://b.example.com,,").collect();

        assert_eq!(urls, ["https://a.example.com", "https://b.example.com"]);
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
                    alt: None,
                })
                .collect(),
            stats: None,
            lang: None,
            reply_to: None,
            quote: None,
            translation: None,
        }
    }

    #[test]
    fn media_links_end_in_the_extension_of_animated_images() {
        let mut embed = post(&[
            "https://i.redd.it/nhvtlii5suxh1.gif",
            "https://i.redd.it/v1eq2vzfk1zh1.jpeg",
        ]);
        embed.reply_to = Some(Box::new(post(&[
            "https://gif.abembed.com/tweet_video/HTNOycZXIAAn9i8.webp?s=1",
            "https://packaged-media.redd.it/a/m2-res_720p.mp4?m=DASHPlaylist.mpd",
        ])));

        assert_eq!(
            (0..5)
                .map(|index| media_file(&embed, index))
                .collect::<Vec<_>>(),
            ["0.gif", "1", "2.webp", "3", "4"]
        );
    }

    #[test]
    fn media_links_route_with_or_without_an_extension() {
        assert_eq!(media_index("2"), Some(2));
        assert_eq!(media_index("2.webp"), Some(2));
        assert_eq!(media_index("webp"), None);
        assert_eq!(media_index(""), None);
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
