#[cfg(test)]
mod fixtures;
mod page;

use super::{
    bad_request, get_cached, html::Site, post_cache, put_cached, redirect, redirects_browser,
    sized, warn, EmbedPost, HttpError, Providers, Source,
};
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    response::Response,
    routing::get,
    Router,
};
use page::{author, dash_id, dash_video, oembed};
use serde::Deserialize;
use serde_json::json;
use worker::{Env, Url};

const HTML_HEADERS: [(&str, &str); 13] = [
    (
        "Accept",
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/jxl,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7",
    ),
    ("Accept-Language", "en"),
    ("DNT", "1"),
    ("Priority", "u=0, i"),
    (
        "Sec-CH-UA",
        r#""Not:A-Brand";v="8", "Chromium";v="156", "Google Chrome";v="156""#,
    ),
    ("Sec-CH-UA-Mobile", "?0"),
    ("Sec-CH-UA-Platform", r#""Linux""#),
    ("Sec-Fetch-Dest", "document"),
    ("Sec-Fetch-Mode", "navigate"),
    ("Sec-Fetch-Site", "none"),
    ("Sec-Fetch-User", "?1"),
    ("Upgrade-Insecure-Requests", "1"),
    (
        "User-Agent",
        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/156.0.0.0 Safari/537.36",
    ),
];

const OEMBED_HEADERS: [(&str, &str); 2] = [
    ("Accept", "application/json"),
    (
        "User-Agent",
        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36",
    ),
];

const POST_PATHS: [&str; 4] = [
    "/r/{subreddit}/comments/{id}",
    "/r/{subreddit}/comments/{id}/{slug}",
    "/r/{subreddit}/comments/{id}/comment/{comment}",
    "/r/{subreddit}/comments/{id}/{slug}/{comment}",
];

pub fn get_router() -> Router<Env> {
    POST_PATHS
        .iter()
        .fold(
            Router::new().route("/reddit", get(crate::root)),
            |router, path| {
                router
                    .route(&format!("/reddit{path}"), get(embed))
                    .route(&format!("/api/reddit{path}"), get(post_api))
            },
        )
        .route("/reddit/r/{subreddit}/s/{code}", get(share_embed))
        .route("/api/reddit/r/{subreddit}/s/{code}", get(share_post_api))
        .route(
            "/reddit/r/{subreddit}/comments/{id}/media/{index}",
            get(media_api),
        )
        .route(
            "/reddit/r/{subreddit}/comments/{id}/comment/{comment}/media/{index}",
            get(media_api),
        )
}

#[derive(Deserialize)]
struct PostPath {
    subreddit: String,
    id: String,
    slug: Option<String>,
    comment: Option<String>,
}

impl PostPath {
    fn source(&self) -> Result<RedditSource, HttpError> {
        RedditSource::new(
            &self.subreddit,
            &self.id,
            self.slug.as_deref(),
            self.comment.as_deref(),
        )
    }
}

#[derive(Deserialize)]
struct MediaPath {
    subreddit: String,
    id: String,
    comment: Option<String>,
    index: usize,
}

#[worker::send]
async fn embed(
    State(env): State<Env>,
    Path(path): Path<PostPath>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    super::embed(&env, &path.source()?, &headers).await
}

#[worker::send]
async fn post_api(
    State(env): State<Env>,
    Path(path): Path<PostPath>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    super::api(&env, &path.source()?, &headers).await
}

#[worker::send]
async fn share_embed(
    State(env): State<Env>,
    Path((subreddit, code)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    if redirects_browser(&env, &headers) {
        return Ok(redirect(&share_url(&subreddit, &code)?));
    }

    let source = resolve_share_link(&env, &subreddit, &code).await?;
    super::embed(&env, &source, &headers).await
}

#[worker::send]
async fn share_post_api(
    State(env): State<Env>,
    Path((subreddit, code)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let source = resolve_share_link(&env, &subreddit, &code).await?;
    super::api(&env, &source, &headers).await
}

fn share_url(subreddit: &str, code: &str) -> Result<String, HttpError> {
    if !is_subreddit(subreddit) || !sized(code, 4..=32, |byte| byte.is_ascii_alphanumeric()) {
        return Err(bad_request("Unsupported Reddit share link"));
    }
    Ok(format!("https://reddit.com/r/{subreddit}/s/{code}"))
}

async fn resolve_share_link(
    env: &Env,
    subreddit: &str,
    code: &str,
) -> Result<RedditSource, HttpError> {
    let share_url = share_url(subreddit, code)?;
    let cache = post_cache(env)?;
    let key = format!("embed-alias:v1:reddit:{code}");

    match get_cached::<String>(&cache, &key).await {
        Some(url) => RedditSource::from_url(&url),
        None => {
            let source = Providers::hosted(env)?
                .fetch_redirect(&share_url, &HTML_HEADERS, |location| {
                    RedditSource::from_url(location).ok()
                })
                .await?;
            put_cached(&cache, &key, &source.source_url()).await;
            Ok(source)
        }
    }
}

#[worker::send]
async fn media_api(
    State(env): State<Env>,
    Path(path): Path<MediaPath>,
) -> Result<Response, HttpError> {
    let source = RedditSource::new(&path.subreddit, &path.id, None, path.comment.as_deref())?;
    super::media(&env, &source, path.index).await
}

#[derive(Clone)]
pub struct RedditSource {
    subreddit: String,
    id: String,
    slug: Option<String>,
    comment: Option<String>,
}

impl RedditSource {
    fn new(
        subreddit: &str,
        id: &str,
        slug: Option<&str>,
        comment: Option<&str>,
    ) -> Result<Self, HttpError> {
        if !is_subreddit(subreddit) && is_thing_id(id) && comment.is_none_or(is_thing_id) {
            return Err(bad_request("Unsupported Reddit post path"));
        }

        Ok(Self {
            subreddit: subreddit.to_string(),
            id: id.to_string(),
            slug: slug.map(str::to_string),
            comment: comment.map(str::to_string),
        })
    }

    fn from_url(url: &str) -> Result<Self, HttpError> {
        let unsupported = || bad_request("Unsupported Reddit post link");
        let url = Url::parse(url).map_err(|_| unsupported())?;
        if url.scheme() != "https" || url.host_str() != Some("www.reddit.com") {
            return Err(unsupported());
        }
        let segments: Vec<&str> = url
            .path_segments()
            .ok_or_else(unsupported)?
            .filter(|segment| !segment.is_empty())
            .collect();

        match segments[..] {
            ["r", subreddit, "comments", id] => Self::new(subreddit, id, None, None),
            ["r", subreddit, "comments", id, "comment", comment] => {
                Self::new(subreddit, id, None, Some(comment))
            }
            ["r", subreddit, "comments", id, slug] => Self::new(subreddit, id, Some(slug), None),
            ["r", subreddit, "comments", id, slug, comment] => {
                Self::new(subreddit, id, Some(slug), Some(comment))
            }
            _ => Err(unsupported()),
        }
    }

    fn path(&self) -> String {
        let post = format!("/r/{}/comments/{}/", self.subreddit, self.id);
        match &self.comment {
            Some(comment) => format!("{post}comment/{comment}/"),
            None => post,
        }
    }

    fn post(&self) -> Self {
        Self {
            comment: None,
            ..self.clone()
        }
    }

    async fn fetch_page(&self, providers: &Providers) -> Result<EmbedPost, HttpError> {
        let url = format!("https://embed.reddit.com{}", self.path());
        providers
            .fetch(&url, &HTML_HEADERS, |html| self.embed_from_html(html))
            .await
    }

    async fn post_title(&self, providers: &Providers) -> Option<String> {
        match self.post().fetch_page(providers).await {
            Ok(post) => post.title,
            Err(HttpError(_, message)) => {
                self.warn_fallback("post", &message);
                None
            }
        }
    }

    async fn oembed(&self, providers: &Providers) -> (Option<String>, Option<String>) {
        let url = Url::parse_with_params(
            "https://www.reddit.com/oembed",
            [("url", self.post().source_url())],
        )
        .unwrap();
        let result = providers.fetch(url.as_str(), &OEMBED_HEADERS, oembed).await;
        result.unwrap_or_else(|HttpError(_, message)| {
            self.warn_fallback("oembed", &message);
            (None, None)
        })
    }

    fn slug_title(&self) -> Option<String> {
        self.slug
            .as_deref()
            .filter(|slug| {
                slug.bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            })
            .map(|slug| slug.replace('_', " "))
    }

    fn warn_fallback(&self, step: &str, message: &str) {
        warn(&json!({
            "event": step,
            "platform": "reddit",
            "id": self.id(),
            "error": message,
        }));
    }
}

impl Source for RedditSource {
    const PLATFORM: &'static str = "reddit";
    const SITE: Site = Site {
        name: "Reddit",
        accent_color: 0xff4500,
    };
    const HOSTS: &'static [&'static str] = &["abreddit.com"];
    const FORCE_FULL_EMBED: bool = true;

    fn id(&self) -> &str {
        self.comment.as_deref().unwrap_or(&self.id)
    }

    fn cache_key(&self) -> String {
        let key = format!(
            "embed:v1:reddit:{}:{}",
            self.subreddit.to_ascii_lowercase(),
            self.id
        );

        match &self.comment {
            Some(comment) => format!("{key}:{comment}"),
            None => key,
        }
    }

    fn source_url(&self) -> String {
        format!("https://www.reddit.com{}", self.path())
    }

    fn media_url(&self, base: &str, index: usize) -> String {
        format!("{base}{}media/{index}", self.path())
    }

    // reddit embeds have no author avatars - ABE-23
    fn avatar_url(&self, _base: &str) -> String {
        String::new()
    }

    async fn fetch(&self, _env: &Env, providers: &Providers) -> Result<EmbedPost, HttpError> {
        let mut embed = self.fetch_page(providers).await?;

        for media in &mut embed.media {
            if let Some(id) = dash_id(&media.url).map(str::to_string) {
                let playlist = format!("https://v.redd.it/{id}/DASHPlaylist.mpd");
                *media = providers
                    .fetch(&playlist, &HTML_HEADERS, |xml| dash_video(xml, &id))
                    .await?;
            }
        }

        if self.comment.is_some() {
            embed.title = self.post_title(providers).await;
        }

        if embed.title.is_none() {
            let (title, author_name) = self.oembed(providers).await;
            embed.title = title.or_else(|| self.slug_title());
            // oembed is always for the post, its author is not the commenter
            if self.comment.is_none() {
                embed.author = embed.author.or(author_name.map(author));
            }
        }

        let heading = embed.title.as_ref().map(|title| match self.comment {
            Some(_) => format!("**Re: {title}**"),
            None => format!("**{title}**"),
        });

        embed.description = match (heading, embed.description.take()) {
            (Some(heading), Some(body)) => Some(format!("{heading}\n{body}")),
            (heading, body) => heading.or(body),
        };

        Ok(embed)
    }
}

fn is_subreddit(subreddit: &str) -> bool {
    sized(subreddit, 2..=21, |byte| {
        byte.is_ascii_alphanumeric() || byte == b'_'
    })
}

// base36 post and comment ids
fn is_thing_id(id: &str) -> bool {
    sized(id, 1..=16, |byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit()
    })
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn links_back_to_reddit_and_our_media_routes() {
        assert_eq!(
            source().source_url(),
            "https://www.reddit.com/r/osugame/comments/1xm08ih/"
        );
        assert_eq!(
            comment_source().media_url("https://abreddit.com", 0),
            "https://abreddit.com/r/osugame/comments/1xm08ih/comment/19roziotq9zur/media/0"
        );
    }

    #[test]
    fn posts_and_comments_are_cached_separately() {
        assert_ne!(source().cache_key(), comment_source().cache_key());
        assert_eq!(
            RedditSource::new("OsuGame", POST_ID, None, None)
                .unwrap()
                .cache_key(),
            source().cache_key()
        );
    }

    #[test]
    fn titles_fall_back_to_the_slug() {
        let source =
            RedditSource::new(SUBREDDIT, POST_ID, Some("pippi_x_furina_yuri"), None).unwrap();

        assert_eq!(source.slug_title().as_deref(), Some("pippi x furina yuri"));
    }

    #[test]
    fn share_links_resolve_to_posts_and_comments() {
        let post = "https://www.reddit.com/r/osugame/comments/1xm08ih/some_title/?share_id=x&utm_source=share";
        let comment = "https://www.reddit.com/r/osugame/comments/1xm08ih/comment/19roziotq9zur/";

        assert_eq!(
            RedditSource::from_url(post).unwrap().cache_key(),
            source().cache_key()
        );
        assert_eq!(
            RedditSource::from_url(comment).unwrap().cache_key(),
            comment_source().cache_key()
        );
    }

    #[test]
    fn share_links_reject_other_redirects() {
        for url in [
            "https://www.reddit.com/login/",
            "https://evil.com/r/osugame/comments/1xm08ih/",
            "https://www.reddit.com/r/osugame/s/G2xAHCXuWa",
        ] {
            assert!(RedditSource::from_url(url).is_err(), "{url}");
        }
    }

    #[test]
    fn text_posts_embed_without_a_gallery() {
        let embed = source().embed_from_html(&text_post_page()).unwrap();
        let body = source().embed_html(&embed, "https://abreddit.com");

        assert!(body.contains(r#"id="discord:component-embed""#));
        assert!(!body.contains(r#""type":12"#));
    }
}
