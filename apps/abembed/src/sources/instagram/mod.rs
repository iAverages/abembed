#[cfg(test)]
mod fixtures;
mod page;

use super::{
    bad_request, gallery_html,
    html::{escape_html, meta_name, meta_property, Site},
    log, EmbedPost, HttpError, ProviderResponse, Providers, Source,
};
use crate::accounts::{clears_cookie, with_account, AccountFields, Attempt};
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    response::Response,
    routing::get,
    Router,
};
use serde_json::json;
use worker::{Env, RequestRedirect};

const BROWSER_USER_AGENT: &str = "Mozilla/5.0";
pub const ACCOUNT_FIELDS: AccountFields = AccountFields {
    account_id_cookie: "ds_user_id",
    required_cookies: &["sessionid"],
    cookies: &[
        "sessionid",
        "ds_user_id",
        "csrftoken",
        "mid",
        "ig_did",
        "datr",
        "rur",
    ],
    refreshed_cookies: &["sessionid", "ds_user_id", "csrftoken"],
    headers: &[
        "user-agent",
        "accept-language",
        "sec-ch-ua",
        "sec-ch-ua-platform",
        "sec-ch-ua-mobile",
    ],
};

pub fn get_router() -> Router<Env> {
    Router::new()
        .route("/instagram", get(crate::root))
        .route("/instagram/{media_type}/{shortcode}", get(media))
        .route(
            "/instagram/{username}/{media_type}/{shortcode}",
            get(user_media),
        )
        .route(
            "/instagram/{media_type}/{shortcode}/media/{index}",
            get(media_api),
        )
        .route(
            "/instagram/{media_type}/{shortcode}/avatar",
            get(avatar_api),
        )
        .route("/api/instagram/{media_type}/{shortcode}", get(post_api))
        // old link locations that do not returned anymore, kept to keep old embeds working
        .route(
            "/api/instagram/{media_type}/{shortcode}/media/{index}",
            get(media_api),
        )
}

#[worker::send]
async fn post_api(
    State(env): State<Env>,
    Path((media_type, shortcode)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let source = InstagramSource::new(&media_type, &shortcode)?;
    super::api(&env, &source, &headers).await
}

#[worker::send]
async fn media_api(
    State(env): State<Env>,
    Path((media_type, shortcode, index)): Path<(String, String, usize)>,
) -> Result<Response, HttpError> {
    let source = InstagramSource::new(&media_type, &shortcode)?;
    super::media(&env, &source, index).await
}

#[worker::send]
async fn avatar_api(
    State(env): State<Env>,
    Path((media_type, shortcode)): Path<(String, String)>,
) -> Result<Response, HttpError> {
    let source = InstagramSource::new(&media_type, &shortcode)?;
    super::avatar(&env, &source).await
}

#[worker::send]
async fn media(
    State(env): State<Env>,
    Path((media_type, shortcode)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let source = InstagramSource::new(&media_type, &shortcode)?;
    super::embed(&env, &source, &headers).await
}

#[worker::send]
async fn user_media(
    State(env): State<Env>,
    Path((_, media_type, shortcode)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    media(State(env), Path((media_type, shortcode)), headers).await
}

fn classify_account_page(
    source: &InstagramSource,
    response: &ProviderResponse,
) -> Attempt<EmbedPost> {
    if clears_cookie(&response.set_cookies, "sessionid") {
        return Attempt::AuthFailed("session cookie deleted".to_string());
    }

    if (300..400).contains(&response.status) {
        let location = response.location.as_deref().unwrap_or_default();
        return if location.contains("/accounts/login") {
            Attempt::AuthFailed("redirected to login".to_string())
        } else if location.contains("/challenge") {
            Attempt::AuthFailed("redirected to challenge".to_string())
        } else {
            Attempt::Failed(format!("redirected to {location}"))
        };
    }

    if !(200..300).contains(&response.status) {
        return Attempt::Failed(format!("returned {}", response.status));
    }

    match source.embed_from_html(&response.body) {
        Ok(embed) if !embed.media.is_empty() => Attempt::Success(embed),
        Ok(embed) => Attempt::Failed(format!(
            "still no media ({})",
            embed.title.unwrap_or_default()
        )),
        Err(HttpError(_, message)) => Attempt::Failed(message),
    }
}

async fn resolve_with_account(
    env: &Env,
    source: &InstagramSource,
    providers: &Providers,
) -> Option<EmbedPost> {
    with_account(
        env,
        providers,
        "instagram",
        &source.source_url(),
        &[
            ("Accept", "text/html,application/xhtml+xml"),
            ("Sec-Fetch-Dest", "document"),
            ("Sec-Fetch-Mode", "navigate"),
            ("Sec-Fetch-Site", "none"),
        ],
        RequestRedirect::Manual,
        |response| classify_account_page(source, response),
    )
    .await
}

pub struct InstagramSource {
    media_type: String,
    shortcode: String,
}

impl InstagramSource {
    fn new(media_type: &str, shortcode: &str) -> Result<Self, HttpError> {
        if !matches!(media_type, "p" | "reel" | "reels" | "tv") || !is_shortcode(shortcode) {
            return Err(bad_request("Unsupported Instagram media path"));
        }

        Ok(Self {
            media_type: media_type.to_string(),
            shortcode: shortcode.to_string(),
        })
    }
}

impl Source for InstagramSource {
    const PLATFORM: &'static str = "instagram";
    const SITE: Site = Site {
        name: "Instagram",
        accent_color: 0xce0071,
    };
    const HOSTS: &'static [&'static str] = &["abinstagram.com"];

    fn id(&self) -> &str {
        &self.shortcode
    }

    fn cache_key(&self) -> String {
        format!("embed:v1:instagram:{}", self.shortcode)
    }

    fn source_url(&self) -> String {
        format!("https://www.instagram.com/p/{}/", self.shortcode)
    }

    fn redirect_url(&self) -> String {
        format!(
            "https://www.instagram.com/{}/{}/",
            self.media_type, self.shortcode
        )
    }

    fn media_url(&self, base: &str, index: usize) -> String {
        format!(
            "{base}/{}/{}/media/{index}",
            self.media_type, self.shortcode
        )
    }

    fn avatar_url(&self, base: &str) -> String {
        format!("{base}/{}/{}/avatar", self.media_type, self.shortcode)
    }

    async fn fetch(&self, env: &Env, providers: &Providers) -> Result<EmbedPost, HttpError> {
        let anonymous = providers
            .fetch(
                &self.source_url(),
                &[
                    ("Accept", "text/html,application/xhtml+xml"),
                    ("User-Agent", BROWSER_USER_AGENT),
                ],
                |html| self.embed_from_html(html),
            )
            .await;
        match anonymous {
            Ok(post) if !post.media.is_empty() => Ok(post),
            anonymous => match resolve_with_account(env, self, providers).await {
                Some(authed) => {
                    log(
                        &json!({ "event": "account_resolved", "platform": "instagram", "id": self.shortcode }),
                    );
                    Ok(authed)
                }
                None => anonymous,
            },
        }
    }

    fn embed_html(&self, post: &EmbedPost, base: &str) -> String {
        if post.media.is_empty() {
            return gated_html(post);
        }
        gallery_html(self, post, base)
    }
}

fn is_shortcode(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn gated_html(embed: &EmbedPost) -> String {
    let title = embed.title.as_deref().unwrap_or("Instagram");
    let description = embed.description.as_deref().unwrap_or_default();
    let meta = [
        meta_property("twitter:card", "summary"),
        meta_property("og:type", "article"),
        meta_property("og:title", title),
        meta_property("twitter:title", title),
        meta_property("og:description", description),
        meta_property("twitter:description", description),
        meta_property("og:site_name", "Instagram"),
        meta_property("og:url", &embed.source_url),
        meta_name("theme-color", "#CE0071"),
    ]
    .concat();

    format!(
        r#"<!doctype html><html><head>{meta}</head><body><a href="{}">{}</a></body></html>"#,
        escape_html(&embed.source_url),
        escape_html(title),
    )
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use crate::sources::gated_error;
    use axum::http::StatusCode;
    use serde_json::json;

    fn page(status: u16, location: Option<&str>, body: &str) -> ProviderResponse {
        ProviderResponse {
            status,
            location: location.map(str::to_string),
            set_cookies: Vec::new(),
            body: body.to_string(),
        }
    }

    #[test]
    fn treats_login_and_challenge_redirects_as_auth_failures() {
        let source = source("p");
        for location in [
            "https://www.instagram.com/accounts/login/?next=%2Fp%2FAbC_123%2F",
            "https://www.instagram.com/challenge/?next=/p/AbC_123/",
        ] {
            assert!(matches!(
                classify_account_page(&source, &page(302, Some(location), "")),
                Attempt::AuthFailed(_)
            ));
        }
    }

    #[test]
    fn treats_a_deleted_session_cookie_as_an_auth_failure() {
        let mut response = page(302, Some("https://www.instagram.com/"), "");
        response.set_cookies = vec![
            "sessionid=deleted; expires=Thu, 01-Jan-1970 00:00:01 GMT; Max-Age=-1791466010; path=/"
                .to_string(),
        ];

        assert!(matches!(
            classify_account_page(&source("p"), &response),
            Attempt::AuthFailed(_)
        ));
    }

    #[test]
    fn moves_on_without_reauth_for_other_failures() {
        let source = source("p");
        for response in [
            page(429, None, r#"{"message":"Please wait a few minutes"}"#),
            page(301, Some("https://www.instagram.com/p/AbC_123/"), ""),
            page(200, None, &gated_page()),
            page(200, None, &unavailable_page()),
        ] {
            assert!(matches!(
                classify_account_page(&source, &response),
                Attempt::Failed(_)
            ));
        }
    }

    #[test]
    fn accepts_pages_with_media() {
        let Attempt::Success(embed) =
            classify_account_page(&source("p"), &page(200, None, &carousel_html()))
        else {
            panic!("expected success");
        };

        assert_eq!(media_urls(&embed), [IMAGE_ONE, IMAGE_TWO]);
    }

    #[test]
    fn builds_og_embed_for_gated_posts() {
        let html = html_with_scripts(&[json!({ "xig_polaris_media": {
            "code": SHORTCODE,
            "if_not_gated_logged_out": null,
            "gating_ruling": {
                "gating_type": 3,
                "title": "Age-restricted content",
                "description": "This content is age-restricted based on your age or account settings.",
            },
        } })]);
        let embed = source("p").embed_from_html(&html).unwrap();
        let body = source("p").embed_html(&embed, "https://example.com/instagram");

        assert!(embed.media.is_empty());
        assert!(body.contains(r#"<meta property="og:title" content="Age-restricted content">"#));
        assert!(body.contains(r#"<meta property="og:description" content="This content is age-restricted based on your age or account settings.">"#));
        assert_eq!(
            gated_error(&embed),
            Some(HttpError(
                StatusCode::FORBIDDEN,
                "Age-restricted content".to_string()
            ))
        );
    }

    #[test]
    fn builds_component_embed_for_carousels() {
        let embed = source("reel").embed_from_html(&carousel_html()).unwrap();
        let body = source("reel").embed_html(&embed, "https://example.com/instagram");

        assert!(body.contains(r#"id="discord:component-embed""#));
    }
}
