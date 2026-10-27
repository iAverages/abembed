#[cfg(test)]
mod fixtures;
mod page;
mod translate;

use super::{
    bad_gateway, bad_request, html::Site, json_response, load_post, log, media_base, post_json,
    warn, EmbedPost, HttpError, Media, ProviderResponse, Providers, Source, REGIONS,
};
use crate::accounts::{with_account, AccountFields, Attempt};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::get,
    Router,
};
use page::{
    card_media, card_vmap_url, post_from_graphql, post_from_tweet, related_posts, text_from_page,
};
use serde_json::{json, Value};
use std::{f64::consts::PI, iter, ops::RangeInclusive};
use worker::{Env, RequestRedirect, Url};

const JSON_HEADERS: [(&str, &str); 2] = [
    ("Accept", "application/json"),
    ("User-Agent", "Mozilla/5.0"),
];
const HTML_HEADERS: [(&str, &str); 2] = [
    ("Accept", "text/html,application/xhtml+xml"),
    ("User-Agent", "Mozilla/5.0"),
];
const SYNDICATION_URL: &str = "https://cdn.syndication.twimg.com/tweet-result";
// the web client's public bearer, the account cookies carry the login
pub(super) const BEARER: &str = "Bearer AAAAAAAAAAAAAAAAAAAAANRILgAAAAAAnNwIzUejRCOuH5E6I8xnZz4puTs%3D1Zv7ttfk8LF81IUq16cHjhLTvJu4FA33AGWWjCpTnA";
// the graphql status query the web client runs, ids and flags from a browser capture
const STATUS_QUERY_PATH: &str = "/graphql/f2sagi1jweVHFkTUIHzmMQ/TweetResultByRestId";
const STATUS_FEATURES: &str = r#"{"rweb_video_screen_enabled":false,"profile_label_improvements_pcf_label_in_post_enabled":true,"responsive_web_profile_redirect_enabled":false,"rweb_tipjar_consumption_enabled":false,"verified_phone_label_enabled":false,"creator_subscriptions_tweet_preview_api_enabled":true,"responsive_web_graphql_timeline_navigation_enabled":true,"responsive_web_graphql_skip_user_profile_image_extensions_enabled":false,"premium_content_api_read_enabled":false,"communities_web_enable_tweet_community_results_fetch":true,"c9s_tweet_anatomy_moderator_badge_enabled":true,"responsive_web_grok_analyze_button_fetch_trends_enabled":false,"responsive_web_grok_analyze_post_followups_enabled":true,"responsive_web_jetfuel_frame":true,"responsive_web_grok_share_attachment_enabled":true,"responsive_web_grok_annotations_enabled":true,"articles_preview_enabled":true,"responsive_web_edit_tweet_api_enabled":true,"graphql_is_translatable_rweb_tweet_is_translatable_enabled":true,"view_counts_everywhere_api_enabled":true,"longform_notetweets_consumption_enabled":true,"responsive_web_twitter_article_tweet_consumption_enabled":true,"content_disclosure_indicator_enabled":true,"content_disclosure_ai_generated_indicator_enabled":true,"responsive_web_grok_show_grok_translated_post":true,"responsive_web_grok_analysis_button_from_backend":true,"post_ctas_fetch_enabled":true,"freedom_of_speech_not_reach_fetch_enabled":true,"standardized_nudges_misinfo":true,"tweet_with_visibility_results_prefer_gql_limited_actions_policy_enabled":true,"longform_notetweets_rich_text_read_enabled":true,"longform_notetweets_inline_media_enabled":true,"responsive_web_grok_image_annotation_enabled":true,"responsive_web_grok_imagine_annotation_enabled":true,"responsive_web_grok_community_note_auto_translation_is_enabled":true,"responsive_web_enhance_cards_enabled":false,"tweet_awards_web_tipping_enabled":false}"#;
const STATUS_FIELD_TOGGLES: &str = r#"{"withArticleRichContentState":true,"withArticlePlainText":false,"withGrokAnalyze":false,"withDisallowedReplyControls":false}"#;
// the syndication api cuts note tweets off around here
const NOTE_TWEET_LENGTH: usize = 270;

// accounts fetch what syndication won't serve and translate, see `account_post` and `translate`
pub const ACCOUNT_FIELDS: AccountFields = AccountFields {
    account_id_cookie: "twid",
    required_cookies: &["auth_token", "ct0"],
    cookies: &["auth_token", "ct0", "twid", "kdt", "guest_id"],
    refreshed_cookies: &["auth_token", "ct0", "kdt"],
    headers: &[
        "user-agent",
        "accept-language",
        "sec-ch-ua",
        "sec-ch-ua-platform",
        "sec-ch-ua-mobile",
        "x-client-uuid",
    ],
    csrf_header: Some(("ct0", "x-csrf-token")),
};

pub fn get_router() -> Router<Env> {
    Router::new()
        .route("/twitter", get(crate::root))
        .route("/twitter/{username}/status/{id}", get(media))
        .route(
            "/twitter/{username}/status/{id}/photo/{index}",
            get(media_photo),
        )
        .route(
            "/twitter/{username}/status/{id}/{language}",
            get(translated_media),
        )
        .route(
            "/twitter/{username}/status/{id}/media/{index}",
            get(media_api),
        )
        .route("/twitter/{username}/status/{id}/avatar", get(avatar_api))
        .route(
            "/twitter/{username}/status/{id}/avatar/{index}",
            get(entry_avatar_api),
        )
        .route("/api/twitter/{username}/status/{id}", get(post_api))
        .route(
            "/api/twitter/{username}/status/{id}/{language}",
            get(translated_post_api),
        )
}

#[worker::send]
async fn media(
    State(env): State<Env>,
    Path((username, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let source = TwitterSource::new(&username, &id, None)?;
    super::embed(&env, &source, &headers).await
}

#[worker::send]
async fn media_photo(
    State(env): State<Env>,
    // we dont have special handling for /photo/<index> routes
    Path((username, id, _)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let source = TwitterSource::new(&username, &id, None)?;
    super::embed(&env, &source, &headers).await
}

#[worker::send]
async fn translated_media(
    State(env): State<Env>,
    Path((username, id, language)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let source = TwitterSource::new(&username, &id, Some(&language))?;
    super::embed(&env, &source, &headers).await
}

#[worker::send]
async fn media_api(
    State(env): State<Env>,
    Path((username, id, index)): Path<(String, String, usize)>,
) -> Result<Response, HttpError> {
    let source = TwitterSource::new(&username, &id, None)?;
    super::media(&env, &source, index).await
}

#[worker::send]
async fn avatar_api(
    State(env): State<Env>,
    Path((username, id)): Path<(String, String)>,
) -> Result<Response, HttpError> {
    let source = TwitterSource::new(&username, &id, None)?;
    super::avatar(&env, &source, 0).await
}

#[worker::send]
async fn entry_avatar_api(
    State(env): State<Env>,
    Path((username, id, index)): Path<(String, String, usize)>,
) -> Result<Response, HttpError> {
    let source = TwitterSource::new(&username, &id, None)?;
    super::avatar(&env, &source, index).await
}

#[worker::send]
async fn post_api(
    State(env): State<Env>,
    Path((username, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let source = TwitterSource::new(&username, &id, None)?;
    status_api(&env, &source, &headers).await
}

#[worker::send]
async fn translated_post_api(
    State(env): State<Env>,
    Path((username, id, language)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let source = TwitterSource::new(&username, &id, Some(&language))?;
    status_api(&env, &source, &headers).await
}

async fn status_api(
    env: &Env,
    source: &TwitterSource,
    headers: &HeaderMap,
) -> Result<Response, HttpError> {
    let post = load_post(env, source).await?;
    let base = media_base(TwitterSource::HOSTS, TwitterSource::PLATFORM, headers);
    Ok(json_response(
        StatusCode::OK,
        &source.status_json(&post, &base),
    ))
}

pub struct TwitterSource {
    username: String,
    id: String,
    language: Option<String>,
}

impl TwitterSource {
    fn new(username: &str, id: &str, language: Option<&str>) -> Result<Self, HttpError> {
        let unsupported = || bad_request("Unsupported Twitter status path");
        let username = username.strip_prefix('@').unwrap_or(username);
        let valid = (1..=15).contains(&username.len())
            && username
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            && (1..=20).contains(&id.len())
            && id.bytes().all(|byte| byte.is_ascii_digit())
            && !id.starts_with('0');
        if !valid {
            return Err(unsupported());
        }
        let language = match language {
            Some(language) => Some(normalize_language(language).ok_or_else(unsupported)?),
            None => None,
        };

        Ok(Self {
            username: username.to_string(),
            id: id.to_string(),
            language,
        })
    }

    fn status_json(&self, post: &EmbedPost, base: &str) -> Value {
        let (mut offset, mut index) = (0, 0);
        let mut entry_json = |entry: &EmbedPost| {
            let mut json = post_json(
                entry,
                |media| self.media_url(base, offset + media),
                &self.entry_avatar_url(base, index),
            );
            json["translation"] = json!(entry.translation);
            offset += entry.media.len();
            index += 1;
            json
        };

        let mut json = entry_json(post);
        json["reply_to"] = post
            .reply_to
            .as_deref()
            .map_or(Value::Null, &mut entry_json);
        json["quote"] = post.quote.as_deref().map_or(Value::Null, &mut entry_json);
        json
    }
}

impl TwitterSource {
    async fn syndication_entries(
        &self,
        providers: &Providers,
    ) -> Result<(EmbedPost, Option<EmbedPost>, Option<EmbedPost>), HttpError> {
        let tweet = providers
            .fetch(&syndication_url(&self.id), &JSON_HEADERS, |body| {
                self.tweet_json(body)
            })
            .await?;
        if tweet["__typename"] != "Tweet" {
            return Err(HttpError(
                StatusCode::NOT_FOUND,
                "Twitter status not found".to_string(),
            ));
        }
        let mut post = post_from_tweet(&tweet)
            .ok_or_else(|| bad_gateway("Twitter status is missing its author"))?;
        let (mut reply_to, mut quote) = related_posts(&tweet);

        if post.media.is_empty() {
            post.media
                .extend(card_video(providers, &tweet["card"]).await);
        }
        for entry in iter::once(&mut post)
            .chain(reply_to.as_mut())
            .chain(quote.as_mut())
        {
            fill_note_text(providers, entry).await;
        }
        Ok((post, reply_to, quote))
    }

    async fn account_post(&self, env: &Env, providers: &Providers) -> Option<EmbedPost> {
        let variables = json!({
            "tweetId": self.id,
            "withCommunity": false,
            "includePromotedContent": false,
            "withVoice": false,
        });
        let url = Url::parse_with_params(
            &format!("https://api.x.com{STATUS_QUERY_PATH}"),
            [
                ("variables", variables.to_string().as_str()),
                ("features", STATUS_FEATURES),
                ("fieldToggles", STATUS_FIELD_TOGGLES),
            ],
        )
        .ok()?;
        let result = with_account(
            env,
            providers,
            "twitter",
            url.as_str(),
            &[
                ("Authorization", BEARER),
                ("Accept", "application/json"),
                ("x-twitter-auth-type", "OAuth2Session"),
                ("x-twitter-active-user", "yes"),
            ],
            RequestRedirect::Manual,
            None,
            classify_status,
        )
        .await?;
        post_from_graphql(&result)
    }
}

impl Source for TwitterSource {
    const PLATFORM: &'static str = "twitter";
    const SITE: Site = Site {
        name: "X / Twitter",
        accent_color: 0x1d9bf0,
        stats: &[
            ("💬", |stats| stats.comments),
            ("🔁", |stats| stats.shares),
            ("❤️", |stats| stats.likes),
            ("📊", |stats| stats.views),
        ],
        mention_url: Some("https://x.com/"),
        hashtag_url: Some("https://x.com/hashtag/"),
    };
    const HOSTS: &'static [&'static str] = &["abtwitter.com", "abfixx.com"];
    const FORCE_FULL_EMBED: bool = true;

    fn id(&self) -> &str {
        &self.id
    }

    fn cache_key(&self) -> String {
        match &self.language {
            Some(language) => format!("embed:v1:twitter:{}:{language}", self.id),
            None => format!("embed:v1:twitter:{}", self.id),
        }
    }

    fn source_url(&self) -> String {
        status_url(&self.username, &self.id)
    }

    fn media_url(&self, base: &str, index: usize) -> String {
        format!("{base}/{}/status/{}/media/{index}", self.username, self.id)
    }

    fn avatar_url(&self, base: &str) -> String {
        format!("{base}/{}/status/{}/avatar", self.username, self.id)
    }

    async fn fetch(&self, env: &Env, providers: &Providers) -> Result<EmbedPost, HttpError> {
        let (mut post, mut reply_to, mut quote) = match self.syndication_entries(providers).await {
            Ok(entries) => entries,
            // syndication hides gated statuses, an account still sees them
            Err(error) => match self.account_post(env, providers).await {
                Some(mut post) => {
                    log(
                        &json!({ "event": "account_resolved", "platform": "twitter", "id": self.id }),
                    );
                    let quote = post.quote.take().map(|quote| *quote);
                    (post, None, quote)
                }
                None => return Err(error),
            },
        };

        if let Some(language) = &self.language {
            for entry in iter::once(&mut post)
                .chain(reply_to.as_mut())
                .chain(quote.as_mut())
            {
                translate::translate(env, providers, entry, language).await;
            }
        }
        post.reply_to = reply_to.map(Box::new);
        post.quote = quote.map(Box::new);

        Ok(post)
    }
}

fn classify_status(response: &ProviderResponse) -> Attempt<Value> {
    let body = response.body.chars().take(200).collect::<String>();
    match response.status {
        200..=299 => {
            let mut json: Value = serde_json::from_str(&response.body).unwrap_or_default();
            let result = json["data"]["tweetResult"]["result"].take();
            match result["__typename"].as_str() {
                Some("Tweet" | "TweetWithVisibilityResults") => Attempt::Success(result),
                Some(other) => Attempt::Failed(format!("status is a {other}")),
                None => Attempt::Failed(format!("status lookup returned no status: {body}")),
            }
        }
        401 | 403 => Attempt::AuthFailed(format!(
            "status lookup returned {}: {body}",
            response.status
        )),
        status => Attempt::Failed(format!("status lookup returned {status}: {body}")),
    }
}

async fn card_video(providers: &Providers, card: &Value) -> Option<Media> {
    let vmap = fetch_once(providers, &card_vmap_url(card)?, &HTML_HEADERS).await?;
    card_media(card, &vmap)
}

async fn fill_note_text(providers: &Providers, post: &mut EmbedPost) {
    let truncated = post
        .description
        .as_ref()
        .is_some_and(|description| description.chars().count() >= NOTE_TWEET_LENGTH);
    if !truncated {
        return;
    }
    let Some(html) = fetch_once(providers, &post.source_url, &HTML_HEADERS).await else {
        return;
    };
    if let Some(text) = text_from_page(&html) {
        post.description = Some(text);
    }
}

async fn fetch_once(providers: &Providers, url: &str, headers: &[(&str, &str)]) -> Option<String> {
    let region = providers.used().unwrap_or(REGIONS[0]);
    let response = providers
        .fetch_raw(region, url, headers, RequestRedirect::Follow, None)
        .await
        .inspect_err(|HttpError(_, message)| {
            warn(&json!({ "event": "page", "platform": "twitter", "url": url, "error": message }))
        })
        .ok()?;
    (200..300)
        .contains(&response.status)
        .then_some(response.body)
}

fn normalize_language(value: &str) -> Option<String> {
    let language = value.trim().to_ascii_lowercase().replace('_', "-");
    let language = match language.as_str() {
        "zh" | "cn" | "zh-hans" => "zh-cn",
        "tw" | "hk" | "zh-hk" | "zh-mo" | "zh-hant" => "zh-tw",
        "jp" => "ja",
        "kr" => "ko",
        "ua" => "uk",
        other => other,
    };
    let alpha = |part: &str, len: RangeInclusive<usize>| {
        len.contains(&part.len()) && part.bytes().all(|byte| byte.is_ascii_lowercase())
    };
    let valid = match language.split_once('-') {
        Some((code, region)) => alpha(code, 2..=3) && alpha(region, 2..=4),
        None => alpha(language, 2..=3),
    };
    valid.then(|| language.to_string())
}

fn status_url(username: &str, id: &str) -> String {
    format!("https://x.com/{username}/status/{id}")
}

fn syndication_url(id: &str) -> String {
    Url::parse_with_params(
        SYNDICATION_URL,
        [
            ("id", id),
            ("lang", "en"),
            ("token", &syndication_token(id)),
        ],
    )
    .unwrap()
    .to_string()
}

// no fucking idea
fn syndication_token(id: &str) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let value = id.parse::<f64>().unwrap_or_default() / 1e15 * PI;
    let mut integer = value.trunc();
    let mut fraction = value.fract();
    // half the gap to the next double, scaled with the fraction
    let mut delta = 0.5 * (f64::from_bits(value.to_bits() + 1) - value);
    let mut fraction_digits = Vec::new();
    while fraction >= delta {
        fraction *= 36.0;
        delta *= 36.0;
        let digit = fraction as usize;
        fraction_digits.push(digit);
        fraction -= digit as f64;
        // round the last digit to even, carrying into the digits before it
        if (fraction > 0.5 || (fraction == 0.5 && digit % 2 == 1)) && fraction + delta > 1.0 {
            loop {
                match fraction_digits.pop() {
                    Some(digit) if digit + 1 < 36 => {
                        fraction_digits.push(digit + 1);
                        break;
                    }
                    Some(_) => {}
                    None => {
                        integer += 1.0;
                        break;
                    }
                }
            }
            break;
        }
    }

    let mut integer = integer as u64;
    let mut digits = Vec::new();
    loop {
        digits.push((integer % 36) as usize);
        integer /= 36;
        if integer == 0 {
            break;
        }
    }
    digits.reverse();
    digits.extend(fraction_digits);

    let token: String = digits
        .into_iter()
        .filter(|digit| *digit != 0)
        .map(|digit| char::from(DIGITS[digit]))
        .collect();
    if token.is_empty() {
        "0".to_string()
    } else {
        token
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use crate::sources::{embed_response, Translation};

    #[test]
    fn registers_routes_without_conflicts() {
        let _ = get_router();
    }

    #[test]
    fn validates_usernames_ids_and_languages() {
        let with_at = TwitterSource::new("@EliasLou_st", STATUS_ID, None).unwrap();
        assert_eq!(
            with_at.source_url(),
            format!("https://x.com/EliasLou_st/status/{STATUS_ID}")
        );
        assert_eq!(
            TwitterSource::new("EliasLou_st", STATUS_ID, Some("EN"))
                .unwrap()
                .cache_key(),
            format!("embed:v1:twitter:{STATUS_ID}:en")
        );
        assert_eq!(
            TwitterSource::new("jack", "20", Some("jp"))
                .unwrap()
                .language
                .as_deref(),
            Some("ja")
        );
        for (username, id, language) in [
            ("bad!", STATUS_ID, None),
            ("sixteen_chars_ab", STATUS_ID, None),
            ("jack", "0123", None),
            ("jack", "not-an-id", None),
            ("jack", STATUS_ID, Some("english")),
        ] {
            assert!(
                TwitterSource::new(username, id, language).is_err(),
                "{username} {id} {language:?}"
            );
        }
    }

    #[test]
    fn classifies_status_lookups() {
        let response = |status: u16, body: &str| ProviderResponse {
            status,
            location: None,
            set_cookies: Vec::new(),
            body: body.to_string(),
        };
        let found = json!({ "data": { "tweetResult": { "result": graphql_status() } } });

        assert!(matches!(
            classify_status(&response(200, &found.to_string())),
            Attempt::Success(result) if result["tweet"]["rest_id"] == STATUS_ID
        ));
        assert!(matches!(
            classify_status(&response(
                200,
                r#"{"data":{"tweetResult":{"result":{"__typename":"TweetTombstone"}}}}"#
            )),
            Attempt::Failed(_)
        ));
        assert!(matches!(
            classify_status(&response(200, "{}")),
            Attempt::Failed(_)
        ));
        assert!(matches!(
            classify_status(&response(401, "")),
            Attempt::AuthFailed(_)
        ));
        assert!(matches!(
            classify_status(&response(404, "")),
            Attempt::Failed(_)
        ));
    }

    #[test]
    fn normalizes_language_codes() {
        assert_eq!(normalize_language(" EN ").as_deref(), Some("en"));
        assert_eq!(normalize_language("zh_Hant").as_deref(), Some("zh-tw"));
        assert_eq!(normalize_language("pt-br").as_deref(), Some("pt-br"));
        assert!(normalize_language("english").is_none());
        assert!(normalize_language("e").is_none());
    }

    #[test]
    fn builds_syndication_urls_with_a_token() {
        let url = Url::parse(&syndication_url(STATUS_ID)).unwrap();
        let token = url.query_pairs().find(|(key, _)| key == "token").unwrap().1;
        assert_eq!(url.host_str(), Some("cdn.syndication.twimg.com"));
        assert!(!token.is_empty());
        assert!(!token.contains(['0', '.']));
        // values from running the embed script's formula in node
        assert_eq!(syndication_token("20"), "6dq1a2xwd93");
        assert_eq!(syndication_token(STATUS_ID), "53l8ohafo5u");
    }

    #[test]
    fn builds_discord_embed_with_reply_and_quote() {
        let source = source();
        let mut post = reply_with_quote_post();
        post.quote.as_mut().unwrap().translation = Some(Translation {
            text: "Warme Kekse".to_string(),
            source_lang: "en".to_string(),
            target_lang: "de".to_string(),
            provider: "Grok".to_string(),
        });
        let body = source.embed_html(&post, BASE);
        let components = component(&body)["components"].take();
        let components = components.as_array().unwrap();
        let media_route =
            |index: usize| format!("{BASE}/EliasLou_st/status/{STATUS_ID}/media/{index}");

        assert_eq!(
            components[0]["components"][0]["content"],
            "**Moca Space ([@MocaRelaxSpace](https://x.com/MocaRelaxSpace))**\nFour what?"
        );
        assert_eq!(
            components[0]["accessory"]["media"]["url"],
            format!("{BASE}/EliasLou_st/status/{STATUS_ID}/avatar")
        );
        assert_eq!(components[1]["items"][0]["media"]["url"], media_route(0));
        assert_eq!(components[2]["content"], format!("💬 3 · 🔁 4 · ❤️ 12 · [View on X / Twitter](https://x.com/MocaRelaxSpace/status/{STATUS_ID})"));
        assert_eq!(components[3]["type"], 14);
        assert_eq!(
            components[4]["components"][0]["content"],
            "**Replying to**\n**mari ([@butchtimoris](https://x.com/butchtimoris))**\n> four"
        );
        assert_eq!(
            components[4]["accessory"]["media"]["url"],
            format!("{BASE}/EliasLou_st/status/{STATUS_ID}/avatar/1")
        );
        assert_eq!(components[5]["items"][0]["media"]["url"], media_route(1));
        assert_eq!(components[7]["type"], 14);
        assert_eq!(
            components[8]["content"],
            "**Translation (de)**\n> Warme Kekse"
        );
        assert_eq!(components[9]["type"], 14);
        assert_eq!(
            components[10]["components"][0]["content"],
            "**Quoted post**\n**Food Lover ([@_FoodieX](https://x.com/_FoodieX))**\n> Warm cookies"
        );
        assert_eq!(components[11]["items"][0]["media"]["url"], media_route(2));
        assert_eq!(components.len(), 13);
        assert!(body.contains(&format!(
            r#"<meta property="og:video" content="{}">"#,
            media_route(2)
        )));
        assert!(body.contains(r#"<meta property="og:description" content="Four what?">"#));
        assert!(body.contains(r##"<meta name="theme-color" content="#1D9BF0">"##));
    }

    #[test]
    fn text_only_posts_get_the_full_embed_without_a_gallery() {
        let source = source();
        let post = post_from_tweet(&tweet(
            STATUS_ID,
            "Text Author",
            "text_author",
            "Just words",
            [0, 10],
        ))
        .unwrap();
        let response = embed_response(&post, TwitterSource::FORCE_FULL_EMBED, || {
            source.embed_html(&post, BASE)
        });
        let single = post_from_tweet(&tweet_with_media(
            STATUS_ID,
            "A",
            "a",
            "one",
            [0, 3],
            photo(IMAGE_URL, 1, 1),
        ))
        .unwrap();
        let single = embed_response(&single, TwitterSource::FORCE_FULL_EMBED, || {
            source.embed_html(&single, BASE)
        });

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(single.status(), StatusCode::OK);
        let components = component(&source.embed_html(&post, BASE))["components"].take();
        assert_eq!(components.as_array().unwrap().len(), 2);
        assert!(components.to_string().contains("Just words"));
    }

    #[test]
    fn builds_status_json_with_related_posts() {
        let source = source();
        let mut post = reply_with_quote_post();
        post.translation = Some(Translation {
            text: "Vier was?".to_string(),
            source_lang: "en".to_string(),
            target_lang: "de".to_string(),
            provider: "Workers AI".to_string(),
        });
        let json = source.status_json(&post, BASE);

        assert_eq!(json["platform"], "twitter");
        assert_eq!(json["id"], STATUS_ID);
        assert_eq!(json["author"]["name"], "Moca Space (@MocaRelaxSpace)");
        assert_eq!(
            json["author"]["avatar_url"],
            format!("{BASE}/EliasLou_st/status/{STATUS_ID}/avatar")
        );
        assert_eq!(
            json["stats"],
            json!({ "likes": 12, "reposts": 4, "comments": 3 })
        );
        assert_eq!(
            json["media"][0]["url"],
            format!("{BASE}/EliasLou_st/status/{STATUS_ID}/media/0")
        );
        assert_eq!(json["translation"]["text"], "Vier was?");
        assert_eq!(json["reply_to"]["id"], PARENT_ID);
        assert_eq!(
            json["reply_to"]["media"][0]["url"],
            format!("{BASE}/EliasLou_st/status/{STATUS_ID}/media/1")
        );
        assert_eq!(
            json["reply_to"]["author"]["avatar_url"],
            format!("{BASE}/EliasLou_st/status/{STATUS_ID}/avatar/1")
        );
        assert_eq!(json["reply_to"]["translation"], Value::Null);
        assert_eq!(json["quote"]["media"][0]["type"], "video");
        assert_eq!(
            json["quote"]["media"][0]["url"],
            format!("{BASE}/EliasLou_st/status/{STATUS_ID}/media/2")
        );

        let alone = source.status_json(
            &post_from_tweet(&tweet(STATUS_ID, "A", "a", "alone", [0, 5])).unwrap(),
            BASE,
        );
        assert_eq!(alone["reply_to"], Value::Null);
        assert_eq!(alone["quote"], Value::Null);
        assert_eq!(alone["media"], json!([]));
    }
}
