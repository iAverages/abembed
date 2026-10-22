#[cfg(test)]
mod fixtures;
mod page;

use super::{
    bad_gateway, bad_request, html::Site, log, warn, EmbedPost, HttpError, ProviderResponse,
    Providers, Source, REGIONS,
};
use crate::accounts::{clears_cookie, with_account, AccountFields, Attempt};
use axum::{
    extract::State,
    http::{HeaderMap, Uri},
    response::Response,
    routing::get,
    Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use regex::Regex;
use serde_json::json;
use std::sync::LazyLock;
use worker::{js_sys, Env, RequestRedirect, Url};

static ALBUM_OWNER_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""userID":"(\d+)""#).unwrap());

const FACEBOOK_HOSTS: [&str; 5] = [
    "facebook.com",
    "www.facebook.com",
    "m.facebook.com",
    "web.facebook.com",
    "mbasic.facebook.com",
];

pub const ACCOUNT_FIELDS: AccountFields = AccountFields {
    account_id_cookie: "c_user",
    required_cookies: &["xs"],
    cookies: &["c_user", "xs", "datr", "sb", "fr"],
    refreshed_cookies: &["xs", "fr", "datr", "sb"],
    headers: &[
        "user-agent",
        "accept-language",
        "sec-ch-ua",
        "sec-ch-ua-platform",
        "sec-ch-ua-mobile",
        "viewport-width",
        "dpr",
    ],
};

const DESKTOP_HEADERS: [(&str, &str); 4] = [
    (
        "Accept",
        "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
    ),
    ("Accept-Language", "en-us,en;q=0.5"),
    ("Sec-Fetch-Mode", "navigate"),
    (
        "User-Agent",
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/136.0.0.0 Safari/537.36",
    ),
];

pub fn get_router() -> Router<Env> {
    Router::new()
        .route("/facebook", get(crate::root))
        .route("/facebook/{*path}", get(media))
        .route("/api/facebook/{*path}", get(api))
}

#[worker::send]
async fn media(
    State(env): State<Env>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let path = uri.path().strip_prefix("/facebook").unwrap_or_default();

    let direct = media_index(path)
        .and_then(|(path, index)| Some((FacebookSource::new(path, uri.query()).ok()?, index)));

    if let Some((source, index)) = direct {
        return super::media(&env, &source, index).await;
    }

    let avatar = path
        .strip_suffix("/avatar")
        .and_then(|path| FacebookSource::new(path, uri.query()).ok());

    if let Some(source) = avatar {
        return super::avatar(&env, &source).await;
    }

    let source = FacebookSource::new(path, uri.query())?;
    super::embed(&env, &source, &headers).await
}

#[worker::send]
async fn api(State(env): State<Env>, uri: Uri, headers: HeaderMap) -> Result<Response, HttpError> {
    let path = uri.path().strip_prefix("/api/facebook").unwrap_or_default();
    // media route was moved to outside /api, here to keep old links working
    match media_index(path) {
        Some((path, index)) => {
            let source = FacebookSource::new(path, uri.query())?;
            super::media(&env, &source, index).await
        }
        None => {
            let source = FacebookSource::new(path, uri.query())?;
            super::api(&env, &source, &headers).await
        }
    }
}

fn media_index(path: &str) -> Option<(&str, usize)> {
    let (path, index) = path.rsplit_once('/')?;
    Some((path.strip_suffix("/media")?, index.parse().ok()?))
}

pub struct FacebookSource {
    id: String,
    // normalized facebook.com path
    path: String,
    // params required to identify the post
    query: Vec<(&'static str, String)>,
}

impl FacebookSource {
    fn new(path: &str, query: Option<&str>) -> Result<Self, HttpError> {
        let query = query.map(|query| format!("?{query}")).unwrap_or_default();
        Self::from_url(&format!("https://www.facebook.com{path}{query}")).ok_or_else(unsupported)
    }

    fn from_url(url: &str) -> Option<Self> {
        let url = Url::parse(url).ok()?;
        if url.scheme() != "https" || !FACEBOOK_HOSTS.contains(&url.host_str()?) {
            return None;
        }

        Self::parse(&url)
    }

    fn parse(url: &Url) -> Option<Self> {
        let segments: Vec<&str> = url
            .path_segments()?
            .filter(|segment| !segment.is_empty())
            .collect();
        let param = |name: &str| {
            url.query_pairs()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned())
                .filter(|value| is_id(value))
        };

        // normalised urls, does two things:
        // - ensures the same cache key is used for posts no matter what url is used
        // - ensures we can fetch the page without getting auth walled. Some urls always
        // prompt for a login, even when other urls allow unauthenticated access
        match segments.as_slice() {
            ["share", code] if is_id(code) => Self::at(format!("share/{code}"), code),
            ["share", kind @ ("p" | "r" | "v"), code] if is_id(code) => {
                Self::at(format!("share/{kind}/{code}"), code)
            }
            ["reel", reel] if is_id(reel) => Self::at(format!("reel/{reel}"), reel),
            ["watch"] | ["video.php"] => Self::with_query("watch", [("v", param("v")?)]),
            ["photo"] | ["photo.php"] => {
                let album = url
                    .query_pairs()
                    .find(|(key, value)| key == "set" && value.starts_with("a."))
                    .filter(|(_, value)| is_id(&value[2..]))
                    .map(|(_, value)| ("set", value.into_owned()));
                let fbid = param("fbid")?;
                Some(Self {
                    path: "photo".to_string(),
                    id: fbid.clone(),
                    query: [("fbid", fbid)].into_iter().chain(album).collect(),
                })
            }
            ["permalink.php"] | ["story.php"] => Self::with_query(
                "permalink.php",
                [("story_fbid", param("story_fbid")?), ("id", param("id")?)],
            ),
            ["groups", group, "posts" | "permalink", post] if is_name(group) && is_id(post) => {
                Self::at(format!("groups/{group}/posts/{post}"), post)
            }
            [user, kind @ ("posts" | "videos"), post]
            | [user, kind @ ("posts" | "videos"), _, post]
                if is_name(user) && is_id(post) =>
            {
                Self::at(format!("{user}/{kind}/{post}"), post)
            }
            [user, "photos", album, photo] if is_name(user) && is_name(album) && is_id(photo) => {
                Self::at(format!("{user}/photos/{album}/{photo}"), photo)
            }
            ["stories", bucket, story] if is_id(bucket) => {
                Self::at(format!("stories/{bucket}/{story}"), &story_id(story)?)
            }
            _ => None,
        }
    }

    fn at(path: String, id: &str) -> Option<Self> {
        Some(Self {
            path,
            query: Vec::new(),
            id: id.to_string(),
        })
    }

    fn with_query<const N: usize>(path: &str, query: [(&'static str, String); N]) -> Option<Self> {
        Some(Self {
            path: path.to_string(),
            id: query[0].1.clone(),
            query: query.into(),
        })
    }

    fn query_string(&self) -> String {
        if self.query.is_empty() {
            return String::new();
        }
        let pairs: Vec<String> = self
            .query
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect();
        format!("?{}", pairs.join("&"))
    }

    fn param(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }

    fn owner(&self) -> Option<&str> {
        let segments: Vec<&str> = self.path.split('/').collect();
        match segments.as_slice() {
            ["permalink.php"] => self.param("id"),
            [owner, "photos", _, _] => Some(owner),
            _ => None,
        }
    }

    fn album(&self) -> Option<&str> {
        self.param("set").map(|set| &set[2..])
    }

    fn posts_url(&self, owner: &str) -> String {
        format!("https://www.facebook.com/{owner}/posts/{}/", self.id)
    }

    // hosting/datacenter/server ips only get the login page for photo and permalink links, but
    // `/{owner}/posts/{id}` is served to them with the same media and details. Reels, videos and
    // group posts are fine as they are. A photo inside a multi photo post gets a page without
    // media, fetch then falls back to an account
    async fn anonymous_url(&self, providers: &Providers) -> String {
        let mut target = None;
        if self.path.starts_with("share/") {
            target = providers
                .fetch_redirect(&self.source_url(), &DESKTOP_HEADERS, Self::from_url)
                .await
                .ok();
        }
        let source = target.as_ref().unwrap_or(self);
        let owner = match (source.owner(), source.album()) {
            (Some(owner), _) => Some(owner.to_string()),
            (None, Some(album)) => album_owner(album, providers).await,
            (None, None) => None,
        };
        match owner {
            Some(owner) => source.posts_url(&owner),
            None => source.source_url(),
        }
    }
}

impl Source for FacebookSource {
    const PLATFORM: &'static str = "facebook";
    const SITE: Site = Site {
        name: "Facebook",
        accent_color: 0x0866ff,
    };
    const HOSTS: &'static [&'static str] = &["abfacebook.com"];

    fn id(&self) -> &str {
        &self.id
    }

    fn cache_key(&self) -> String {
        format!("embed:v2:facebook:{}{}", self.path, self.query_string())
    }

    fn source_url(&self) -> String {
        let slash = if self.path.ends_with(".php") { "" } else { "/" };
        format!(
            "https://www.facebook.com/{}{slash}{}",
            self.path,
            self.query_string()
        )
    }

    fn media_url(&self, base: &str, index: usize) -> String {
        format!("{base}/{}/media/{index}{}", self.path, self.query_string())
    }

    fn avatar_url(&self, base: &str) -> String {
        format!("{base}/{}/avatar{}", self.path, self.query_string())
    }

    async fn fetch(&self, env: &Env, providers: &Providers) -> Result<EmbedPost, HttpError> {
        // stories are only served to logged in users
        if self.path.starts_with("stories/") {
            return resolve_with_account(env, self, providers)
                .await
                .ok_or_else(|| bad_gateway("Facebook story needs an account"));
        }

        let url = self.anonymous_url(providers).await;
        let anonymous = providers
            .fetch(&url, &DESKTOP_HEADERS, |html| self.embed_from_html(html))
            .await;
        let embed = match anonymous {
            Ok(embed) => embed,
            Err(error) => match resolve_with_account(env, self, providers).await {
                Some(embed) => {
                    log(
                        &json!({ "event": "account_resolved", "platform": "facebook", "id": self.id }),
                    );
                    embed
                }
                None => return Err(error),
            },
        };
        if embed.stats.is_none() {
            warn(
                &json!({ "event": "page", "platform": "facebook", "id": self.id, "error": "no author or counts" }),
            );
        }
        Ok(embed)
    }
}

async fn resolve_with_account(
    env: &Env,
    source: &FacebookSource,
    providers: &Providers,
) -> Option<EmbedPost> {
    with_account(
        env,
        providers,
        "facebook",
        &source.source_url(),
        &DESKTOP_HEADERS,
        RequestRedirect::Follow,
        |response| classify_account_page(source, response),
    )
    .await
}

async fn album_owner(album: &str, providers: &Providers) -> Option<String> {
    let url = format!("https://www.facebook.com/media/set/?set=a.{album}");
    // one random region, the page is the same everywhere
    let region = REGIONS[(js_sys::Math::random() * REGIONS.len() as f64) as usize];
    match providers
        .fetch_raw(region, &url, &DESKTOP_HEADERS, RequestRedirect::Follow)
        .await
    {
        Ok(response) => ALBUM_OWNER_PATTERN
            .captures(&response.body)
            .map(|captures| captures[1].to_string()),
        Err(HttpError(_, message)) => {
            warn(
                &json!({ "event": "page", "platform": "facebook", "album": album, "error": message }),
            );
            None
        }
    }
}

fn classify_account_page(
    source: &FacebookSource,
    response: &ProviderResponse,
) -> Attempt<EmbedPost> {
    if clears_cookie(&response.set_cookies, "xs") {
        return Attempt::AuthFailed("session cookie deleted".to_string());
    }
    if !(200..300).contains(&response.status) {
        return Attempt::Failed(format!("returned {}", response.status));
    }
    match source.embed_from_html(&response.body) {
        Ok(embed) => Attempt::Success(embed),
        Err(HttpError(_, message)) => {
            if is_login_page(&response.body) {
                Attempt::AuthFailed("served the login page".to_string())
            } else {
                Attempt::Failed(message)
            }
        }
    }
}

fn is_login_page(html: &str) -> bool {
    html.contains("LoginForm") || html.contains("/checkpoint/")
}

// share codes, numeric ids and `pfbid` ids
fn is_id(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

// story tokens are base64 of `S:_ISC:{id}`
fn story_id(token: &str) -> Option<String> {
    let decoded = STANDARD.decode(token).ok()?;
    let (_, id) = std::str::from_utf8(&decoded).ok()?.rsplit_once(':')?;
    is_id(id).then(|| id.to_string())
}

// usernames, group names and album ids like `a.122113073295209780`
fn is_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

fn unsupported() -> HttpError {
    bad_request("Unsupported Facebook post path")
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn registers_routes_without_conflicts() {
        let _ = get_router();
    }

    #[test]
    fn identifies_facebook_post_links() {
        for (link, canonical, id) in [
            (
                "https://www.facebook.com/share/p/1DL7Uac7xc/?mibextid=wwXIfr",
                "https://www.facebook.com/share/p/1DL7Uac7xc/",
                POST_CODE,
            ),
            (
                "https://m.facebook.com/share/r/1MbWc8LmKs/",
                "https://www.facebook.com/share/r/1MbWc8LmKs/",
                REEL_CODE,
            ),
            (
                "https://facebook.com/share/v/1DJnCygSRY",
                "https://www.facebook.com/share/v/1DJnCygSRY/",
                "1DJnCygSRY",
            ),
            (
                "https://www.facebook.com/share/1DTvc3LK3k/",
                "https://www.facebook.com/share/1DTvc3LK3k/",
                SHARE_CODE,
            ),
            (
                "https://www.facebook.com/kuli.y.mas/posts/wellcome-to-mod-circle-/1405946908140934/",
                "https://www.facebook.com/kuli.y.mas/posts/1405946908140934/",
                "1405946908140934",
            ),
            (
                "https://www.facebook.com/scssoft/posts/pfbid02Ab3cD4eF5gH6/?rdid=x",
                "https://www.facebook.com/scssoft/posts/pfbid02Ab3cD4eF5gH6/",
                "pfbid02Ab3cD4eF5gH6",
            ),
            (
                "https://web.facebook.com/kuli.y.mas/videos/some-title/1551678869998267/",
                "https://www.facebook.com/kuli.y.mas/videos/1551678869998267/",
                "1551678869998267",
            ),
            (
                "https://www.facebook.com/kuli.y.mas/photos/a.122113073295209780/122137936929209780/",
                "https://www.facebook.com/kuli.y.mas/photos/a.122113073295209780/122137936929209780/",
                "122137936929209780",
            ),
            (
                "https://www.facebook.com/reel/1551678869998267/?s=single_unit",
                "https://www.facebook.com/reel/1551678869998267/",
                "1551678869998267",
            ),
            (
                "https://www.facebook.com/watch/?v=1551678869998267&ref=sharing",
                "https://www.facebook.com/watch/?v=1551678869998267",
                "1551678869998267",
            ),
            (
                "https://www.facebook.com/video.php?v=1551678869998267",
                "https://www.facebook.com/watch/?v=1551678869998267",
                "1551678869998267",
            ),
            (
                "https://www.facebook.com/photo.php?fbid=122137936929209780&set=a.122113073295209780&id=61586293416608",
                "https://www.facebook.com/photo/?fbid=122137936929209780&set=a.122113073295209780",
                "122137936929209780",
            ),
            (
                "https://www.facebook.com/photo/?fbid=122137936929209780",
                "https://www.facebook.com/photo/?fbid=122137936929209780",
                "122137936929209780",
            ),
            (
                "https://www.facebook.com/permalink.php?story_fbid=pfbid0Xy9&id=61586293416608",
                "https://www.facebook.com/permalink.php?story_fbid=pfbid0Xy9&id=61586293416608",
                "pfbid0Xy9",
            ),
            (
                "https://m.facebook.com/story.php?story_fbid=1405946908140934&id=100064",
                "https://www.facebook.com/permalink.php?story_fbid=1405946908140934&id=100064",
                "1405946908140934",
            ),
            (
                "https://www.facebook.com/groups/some.group/permalink/1234567890/",
                "https://www.facebook.com/groups/some.group/posts/1234567890/",
                "1234567890",
            ),
            (
                "https://www.facebook.com/stories/140936254287261/UzpfSVNDOjE3NTgyMjIyOTg4MTM5MTE=/?bucket_count=9&source=story_tray",
                "https://www.facebook.com/stories/140936254287261/UzpfSVNDOjE3NTgyMjIyOTg4MTM5MTE=/",
                "1758222298813911",
            ),
        ] {
            let source = source(link);
            assert_eq!(source.source_url(), canonical, "{link}");
            assert_eq!(source.id, id, "{link}");
        }
    }

    #[test]
    fn rejects_unsupported_links() {
        for link in [
            "http://www.facebook.com/share/p/1DL7Uac7xc/",
            "https://www.facebook.com.example.com/share/p/1DL7Uac7xc/",
            "https://www.facebook.com/share/x/1DL7Uac7xc/",
            "https://www.facebook.com/share/p/1DL7-Uac7xc/",
            "https://www.facebook.com/share/p/1DL7Uac7xc/extra/",
            "https://www.facebook.com/kuli.y.mas",
            "https://www.facebook.com/kuli.y.mas/about",
            "https://www.facebook.com/kuli.y.mas/posts/",
            "https://www.facebook.com/watch/",
            "https://www.facebook.com/photo.php?fbid=12-34",
            "https://www.facebook.com/permalink.php?story_fbid=1405946908140934",
        ] {
            assert!(FacebookSource::from_url(link).is_none(), "{link}");
        }
    }

    #[test]
    fn parses_route_paths() {
        let post = FacebookSource::new(
            "/kuli.y.mas/posts/wellcome-to-mod-circle-/1405946908140934",
            None,
        )
        .unwrap();
        let watch = FacebookSource::new("/watch", Some("v=1551678869998267&ref=x")).unwrap();
        let HttpError(status, _) = FacebookSource::new("/kuli.y.mas", None).err().unwrap();

        assert_eq!(
            post.source_url(),
            "https://www.facebook.com/kuli.y.mas/posts/1405946908140934/"
        );
        assert_eq!(
            watch.source_url(),
            "https://www.facebook.com/watch/?v=1551678869998267"
        );
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn media_urls_route_back_to_their_post() {
        for link in [
            "https://www.facebook.com/share/p/1DL7Uac7xc/",
            "https://www.facebook.com/kuli.y.mas/posts/1405946908140934/",
            "https://www.facebook.com/watch/?v=1551678869998267",
            "https://www.facebook.com/permalink.php?story_fbid=pfbid0Xy9&id=61586293416608",
        ] {
            let source = source(link);
            let api_url = Url::parse(&source.media_url("https://abembed.com/facebook", 2)).unwrap();
            let path = api_url.path().strip_prefix("/facebook").unwrap();
            let (path, index) = media_index(path).expect(link);
            let routed = FacebookSource::new(path, api_url.query()).unwrap();

            assert_eq!(index, 2);
            assert_eq!(routed.source_url(), link);
        }
        assert!(media_index("/kuli.y.mas/posts/1405946908140934").is_none());
    }

    fn page(status: u16, body: &str) -> ProviderResponse {
        ProviderResponse {
            status,
            location: None,
            set_cookies: Vec::new(),
            body: body.to_string(),
        }
    }

    #[test]
    fn classifies_account_pages() {
        let login = r#"<html><head><title>Facebook</title></head><body><form id="LoginForm"></form></body></html>"#;
        let shell = r#"<html><head><title>Facebook</title></head><body><script>{"two_step_verification.pre_authentication":9}</script></body></html>"#;

        assert!(matches!(
            classify_account_page(&reel(), &page(200, login)),
            Attempt::AuthFailed(_)
        ));
        assert!(matches!(
            classify_account_page(&reel(), &page(200, shell)),
            Attempt::Failed(_)
        ));
        assert!(matches!(
            classify_account_page(&reel(), &page(500, &reel_html())),
            Attempt::Failed(_)
        ));
        assert!(matches!(
            classify_account_page(&reel(), &page(200, &reel_html())),
            Attempt::Success(_)
        ));
    }

    #[test]
    fn names_the_owner_from_the_link() {
        for (link, owner, album) in [
            (
                "https://www.facebook.com/permalink.php?story_fbid=1405946908140934&id=100064",
                Some("100064"),
                None,
            ),
            (
                "https://www.facebook.com/theguardian/photos/a.631632168995040/1563210505837197/",
                Some("theguardian"),
                None,
            ),
            (
                "https://www.facebook.com/photo/?fbid=1563210505837197&set=a.631632168995040",
                None,
                Some("631632168995040"),
            ),
            (
                "https://www.facebook.com/photo/?fbid=1563210505837197&set=pcb.1",
                None,
                None,
            ),
            ("https://www.facebook.com/share/p/1DL7Uac7xc/", None, None),
        ] {
            let source = source(link);
            assert_eq!(source.owner(), owner, "{link}");
            assert_eq!(source.album(), album, "{link}");
        }
        assert_eq!(
            source("https://www.facebook.com/permalink.php?story_fbid=1405946908140934&id=100064")
                .posts_url("100064"),
            "https://www.facebook.com/100064/posts/1405946908140934/"
        );
    }

    #[test]
    fn builds_discord_gallery_for_image_posts() {
        let source = post();
        let embed = source.embed_from_html(&image_post_html(2)).unwrap();
        let body = source.embed_html(&embed, "https://abembed.com/facebook");

        assert!(body.contains(r#"id="discord:component-embed""#));
        assert!(body.contains(&format!(
            "https://abembed.com/facebook/share/p/{POST_CODE}/media/1"
        )));
        assert_eq!(body.matches(r#"<meta property="og:image""#).count(), 2);
        assert!(body.contains(&format!(
            r#"<meta property="og:image" content="https://abembed.com/facebook/share/p/{POST_CODE}/media/0">"#
        )));
        assert!(!body.contains(IMAGE_ONE));
    }
}
