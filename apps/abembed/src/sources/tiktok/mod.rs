#[cfg(test)]
mod fixtures;
mod page;

use super::{
    bad_request, get_cached, html::Site, post_cache, put_cached, redirect, redirects_browser, warn,
    EmbedPost, HttpError, Providers, Source,
};
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    response::Response,
    routing::get,
    Router,
};
use page::{cdn_url, proxied_video_url};
use serde_json::json;
use worker::{Env, Url};

const HTML_HEADERS: [(&str, &str); 2] = [
    ("Accept", "text/html,application/xhtml+xml"),
    ("User-Agent", "Mozilla/5.0"),
];

const APP_HEADERS: [(&str, &str); 2] = [
    ("Accept", "*/*"),
    (
        "User-Agent",
        "com.zhiliaoapp.musically/260103 (Linux; U; Android 13; en_US; Pixel 7)",
    ),
];
const APP_PLAY_HOST: &str = "api16-normal-useast5.us.tiktokv.com";

pub fn get_router() -> Router<Env> {
    Router::new()
        .route("/tiktok", get(crate::root))
        .route("/tiktok/{shortcode}", get(short_media))
        .route("/tiktok/{username}/{post_type}/{id}", get(media))
        .route(
            "/tiktok/{username}/{post_type}/{id}/media/{index}",
            get(media_api),
        )
        .route(
            "/tiktok/{username}/{post_type}/{id}/avatar",
            get(avatar_api),
        )
        .route("/api/tiktok/{shortcode}", get(short_post_api))
        .route("/api/tiktok/{username}/{post_type}/{id}", get(post_api))
        // old link locations that do not returned anymore, kept to keep old embeds working
        .route(
            "/api/tiktok/{username}/{post_type}/{id}/media/{index}",
            get(media_api),
        )
        .route("/tiktok/vm/{shortcode}", get(short_media))
        .route("/tiktok/t/{shortcode}", get(short_media))
}

#[worker::send]
async fn media(
    State(env): State<Env>,
    Path((username, post_type, id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let source = TikTokSource::new(&username, &post_type, &id)?;
    super::embed(&env, &source, &headers).await
}

#[worker::send]
async fn short_media(
    State(env): State<Env>,
    Path(shortcode): Path<String>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    if redirects_browser(&env, &headers) {
        return Ok(redirect(&short_url(&shortcode)?));
    }

    let source = resolve_short_link(&env, &shortcode).await?;
    super::embed(&env, &source, &headers).await
}

#[worker::send]
async fn post_api(
    State(env): State<Env>,
    Path((username, post_type, id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let source = TikTokSource::new(&username, &post_type, &id)?;
    super::api(&env, &source, &headers).await
}

#[worker::send]
async fn short_post_api(
    State(env): State<Env>,
    Path(shortcode): Path<String>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let source = resolve_short_link(&env, &shortcode).await?;
    super::api(&env, &source, &headers).await
}

#[worker::send]
async fn media_api(
    State(env): State<Env>,
    Path((username, post_type, id, index)): Path<(String, String, String, usize)>,
) -> Result<Response, HttpError> {
    let source = TikTokSource::new(&username, &post_type, &id)?;
    super::media(&env, &source, index).await
}

#[worker::send]
async fn avatar_api(
    State(env): State<Env>,
    Path((username, post_type, id)): Path<(String, String, String)>,
) -> Result<Response, HttpError> {
    let source = TikTokSource::new(&username, &post_type, &id)?;
    super::avatar(&env, &source).await
}

fn short_url(shortcode: &str) -> Result<String, HttpError> {
    if !sized(shortcode, 4..=32, |byte| byte.is_ascii_alphanumeric()) {
        return Err(bad_request("Unsupported TikTok short link"));
    }
    Ok(format!("https://vm.tiktok.com/{shortcode}/"))
}

async fn resolve_short_link(env: &Env, shortcode: &str) -> Result<TikTokSource, HttpError> {
    let short_url = short_url(shortcode)?;
    let cache = post_cache(env)?;
    let key = format!("embed-alias:v1:tiktok:{shortcode}");

    return match get_cached::<String>(&cache, &key).await {
        Some(url) => TikTokSource::from_url(&url),
        None => {
            let source = Providers::from_env(env)?
                .fetch_redirect(&short_url, &HTML_HEADERS, |location| {
                    TikTokSource::from_url(location).ok()
                })
                .await?;
            let url = source.source_url();
            put_cached(&cache, &key, &url).await;
            Ok(source)
        }
    };
}

pub struct TikTokSource {
    username: String,
    post_type: &'static str,
    id: String,
}

impl TikTokSource {
    fn new(username: &str, post_type: &str, id: &str) -> Result<Self, HttpError> {
        let username = username.strip_prefix('@').unwrap_or(username);
        let post_type = ["video", "photo"]
            .into_iter()
            .find(|known| *known == post_type);

        // tiktok is special and doesnt care about the username, and some
        // shortlinks resolve to `/@/videos/<id>`. allow empty username
        let valid = sized(username, 0..=24, |byte| {
            byte.is_ascii_alphanumeric() || b"._".contains(&byte)
        }) && sized(id, 5..=30, |byte| byte.is_ascii_digit());

        match post_type {
            Some(post_type) if valid => Ok(Self {
                username: username.to_string(),
                post_type,
                id: id.to_string(),
            }),
            _ => Err(bad_request("Unsupported TikTok post path")),
        }
    }

    fn from_url(url: &str) -> Result<Self, HttpError> {
        let unsupported = || bad_request("Unsupported TikTok post link");
        let url = Url::parse(url).map_err(|_| unsupported())?;
        if url.scheme() != "https"
            || !matches!(url.host_str(), Some("tiktok.com" | "www.tiktok.com"))
        {
            return Err(unsupported());
        }
        let segments: Vec<&str> = url
            .path_segments()
            .ok_or_else(unsupported)?
            .filter(|segment| !segment.is_empty())
            .collect();
        let [username, post_type, id] = segments[..] else {
            return Err(unsupported());
        };
        let username = username.strip_prefix('@').ok_or_else(unsupported)?;

        Self::new(username, post_type, id)
    }

    fn embed_page_url(&self) -> String {
        format!("https://www.tiktok.com/embed/v2/{}", self.id)
    }

    async fn resolve_video_page(&self, providers: &Providers) -> Result<EmbedPost, HttpError> {
        let mut embed = providers
            .fetch(&self.source_url(), &HTML_HEADERS, |html| {
                self.video_from_page(html)
            })
            .await?;
        let video = &mut embed.media[0];
        if video.url.starts_with(&format!("https://{APP_PLAY_HOST}/")) {
            video.url = providers
                .fetch_redirect(&video.url, &APP_HEADERS, cdn_url)
                .await?;
        }
        video.url = proxied_video_url(&video.url);

        Ok(embed)
    }
}

impl Source for TikTokSource {
    const PLATFORM: &'static str = "tiktok";
    const SITE: Site = Site {
        name: "TikTok",
        accent_color: 0xfe2c55,
    };
    const HOSTS: &'static [&'static str] = &["abtiktok.com"];

    fn id(&self) -> &str {
        &self.id
    }

    fn cache_key(&self) -> String {
        // video and photo ids can collide
        format!("embed:v1:tiktok:{}:{}", self.post_type, self.id)
    }

    fn source_url(&self) -> String {
        format!(
            "https://www.tiktok.com/@{}/{}/{}",
            self.username, self.post_type, self.id
        )
    }

    fn media_url(&self, base: &str, index: usize) -> String {
        format!(
            "{base}/@{}/{}/{}/media/{index}",
            self.username, self.post_type, self.id
        )
    }

    fn avatar_url(&self, base: &str) -> String {
        format!(
            "{base}/@{}/{}/{}/avatar",
            self.username, self.post_type, self.id
        )
    }

    async fn fetch(&self, _env: &Env, providers: &Providers) -> Result<EmbedPost, HttpError> {
        let embed_page_url = self.embed_page_url();
        match self.post_type {
            "photo" => {
                providers
                    .fetch(&embed_page_url, &HTML_HEADERS, |html| {
                        self.photo_from_embed(html)
                    })
                    .await
            }
            _ => match self.resolve_video_page(providers).await {
                Ok(embed) => Ok(embed),
                Err(HttpError(_, message)) => {
                    warn(&json!({
                        "event": "page",
                        "platform": "tiktok",
                        "id": self.id,
                        "error": message,
                        "fallback": "embed",
                    }));
                    providers
                        .fetch(&embed_page_url, &HTML_HEADERS, |html| {
                            self.video_from_embed(html)
                        })
                        .await
                }
            },
        }
    }
}

fn sized(value: &str, len: std::ops::RangeInclusive<usize>, allowed: fn(u8) -> bool) -> bool {
    len.contains(&value.len()) && value.bytes().all(allowed)
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn identifies_tiktok_post_links() {
        for link in [
            "https://www.tiktok.com/@kopilawak/video/7665179028352945426",
            "https://www.tiktok.com/@evenifiam/photo/7666981398800108813?_r=1&_t=ZN-98PYXxrluUw",
            "https://tiktok.com/@some.user_1/video/7665179028352945426/",
            "https://www.tiktok.com/@/video/7447107620265004318?_r=1&utm_source=short_fallback",
        ] {
            assert!(TikTokSource::from_url(link).is_ok(), "{link}");
        }
        for link in [
            "https://www.tiktok.com/kopilawak/video/7665179028352945426",
            "https://www.tiktok.com/@kopilawak/live/7665179028352945426",
            "https://www.tiktok.com/@kopilawak/video/not-an-id",
            "https://www.tiktok.com.example.com/@kopilawak/video/7665179028352945426",
            "http://www.tiktok.com/@kopilawak/video/7665179028352945426",
        ] {
            assert!(TikTokSource::from_url(link).is_err(), "{link}");
        }
    }

    #[test]
    fn accepts_route_usernames_with_or_without_at() {
        let with_at = TikTokSource::new("@kopilawak", "video", VIDEO_ID).unwrap();
        let without_at = TikTokSource::new("kopilawak", "video", VIDEO_ID).unwrap();

        assert_eq!(with_at.source_url(), without_at.source_url());
        assert_eq!(
            with_at.media_url("https://abembed.com/tiktok", 0),
            format!("https://abembed.com/tiktok/@kopilawak/video/{VIDEO_ID}/media/0")
        );
    }

    #[test]
    fn builds_discord_gallery_for_photo_posts() {
        let source = source("photo", PHOTO_ID);
        let embed = source.photo_from_embed(&photo_embed_page()).unwrap();
        let body = source.embed_html(&embed, "https://abembed.com/tiktok");

        assert!(body.contains(r#"id="discord:component-embed""#));
        assert!(body.contains(&format!(
            "https://abembed.com/tiktok/@{USERNAME}/photo/{PHOTO_ID}/media/1"
        )));
    }
}
