use super::{status_url, TwitterSource};
use crate::sources::{
    bad_gateway, Author, EmbedPost, HttpError, Media, MediaKind, Stats, GIF_SERVICE_URL,
};
use percent_encoding::percent_decode_str;
use scraper::{Html, Selector};
use serde_json::Value;
use worker::Url;

impl TwitterSource {
    pub(super) fn tweet_json(&self, body: &str) -> Result<Value, HttpError> {
        let tweet: Value = serde_json::from_str(body)
            .map_err(|_| bad_gateway("Twitter returned invalid status data"))?;
        if tweet.as_object().is_some_and(|tweet| tweet.is_empty())
            || tweet["__typename"] == "TweetTombstone"
        {
            return Ok(tweet);
        }
        if tweet["__typename"] != "Tweet" || tweet["id_str"] != self.id.as_str() {
            return Err(bad_gateway("Twitter returned invalid status data"));
        }
        Ok(tweet)
    }
}

pub(super) fn post_from_tweet(tweet: &Value) -> Option<EmbedPost> {
    let id = non_empty_str(&tweet["id_str"])?;
    let user = &tweet["user"];
    let username = non_empty_str(&user["screen_name"])?;
    let media = tweet["mediaDetails"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(media_from_details)
        .collect();

    Some(EmbedPost {
        provider: "twitter".to_string(),
        provider_id: id.to_string(),
        source_url: status_url(username, id),
        title: None,
        description: description(
            &tweet["text"],
            &tweet["display_text_range"],
            char::len_utf16,
        ),
        author: Some(Author {
            id: None,
            username: username.to_string(),
            display_name: non_empty_str(&user["name"]).map(str::to_string),
            profile_url: Some(format!("https://x.com/{username}")),
            avatar_url: user["profile_image_url_https"]
                .as_str()
                .and_then(avatar_url),
            verified: user["verified"].as_bool(),
        }),
        media,
        stats: Some(Stats {
            views: None,
            likes: count(&tweet["favorite_count"]),
            comments: count(&tweet["conversation_count"]).or_else(|| count(&tweet["reply_count"])),
            shares: count(&tweet["retweet_count"]),
        }),
        lang: non_empty_str(&tweet["lang"]).map(str::to_string),
        reply_to: None,
        quote: None,
        translation: None,
        created_at: created_at(id),
    })
}

pub(super) fn related_posts(tweet: &Value) -> (Option<EmbedPost>, Option<EmbedPost>) {
    let parent = &tweet["parent"];
    let reply_to = (parent["id_str"] == tweet["in_reply_to_status_id_str"])
        .then(|| post_from_tweet(parent))
        .flatten();
    (reply_to, post_from_tweet(&tweet["quoted_tweet"]))
}

pub(super) fn post_from_graphql(result: &Value) -> Option<EmbedPost> {
    let tweet = if result["__typename"] == "TweetWithVisibilityResults" {
        &result["tweet"]
    } else {
        result
    };
    let legacy = &tweet["legacy"];
    let id = non_empty_str(&legacy["id_str"]).or_else(|| non_empty_str(&tweet["rest_id"]))?;
    let user = &tweet["core"]["user_results"]["result"];
    let username = non_empty_str(&user["core"]["screen_name"])
        .or_else(|| non_empty_str(&user["legacy"]["screen_name"]))?;
    let display_name =
        non_empty_str(&user["core"]["name"]).or_else(|| non_empty_str(&user["legacy"]["name"]));
    let avatar = user["avatar"]["image_url"]
        .as_str()
        .or(user["legacy"]["profile_image_url_https"].as_str());
    // note tweets carry their full text separately
    let note = &tweet["note_tweet"]["note_tweet_results"]["result"]["text"];
    let description = if note.is_string() {
        description(note, &Value::Null, |_| 1)
    } else {
        description(&legacy["full_text"], &legacy["display_text_range"], |_| 1)
    };
    let media = legacy["extended_entities"]["media"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(media_from_details)
        .collect();

    Some(EmbedPost {
        provider: "twitter".to_string(),
        provider_id: id.to_string(),
        source_url: status_url(username, id),
        title: None,
        description,
        author: Some(Author {
            id: None,
            username: username.to_string(),
            display_name: display_name.map(str::to_string),
            profile_url: Some(format!("https://x.com/{username}")),
            avatar_url: avatar.and_then(avatar_url),
            verified: user["is_blue_verified"]
                .as_bool()
                .or_else(|| user["legacy"]["verified"].as_bool()),
        }),
        media,
        stats: Some(Stats {
            views: count(&tweet["views"]["count"]),
            likes: count(&legacy["favorite_count"]),
            comments: count(&legacy["reply_count"]),
            shares: count(&legacy["retweet_count"]),
        }),
        lang: non_empty_str(&legacy["lang"]).map(str::to_string),
        reply_to: None,
        quote: post_from_graphql(&tweet["quoted_status_result"]["result"]).map(Box::new),
        translation: None,
        created_at: created_at(id),
    })
}

// status ids hold the milliseconds since this epoch above their lowest 22 bits
const SNOWFLAKE_EPOCH_MS: u64 = 1_288_834_974_657;
// ids up to this one were handed out in order, before the 4th of november 2010
const LAST_SEQUENTIAL_ID: u64 = 29_700_859_247;

fn created_at(id: &str) -> Option<u64> {
    let id = id.parse::<u64>().ok()?;
    (id > LAST_SEQUENTIAL_ID).then(|| ((id >> 22) + SNOWFLAKE_EPOCH_MS) / 1000)
}

fn avatar_url(url: &str) -> Option<String> {
    image_url(url).map(|url| url.replace("_normal.", "_400x400."))
}

fn description(text: &Value, range: &Value, width: fn(char) -> usize) -> Option<String> {
    let text = non_empty_str(text)?;
    let text: String = match (range[0].as_u64(), range[1].as_u64()) {
        (Some(start), Some(end)) if start <= end => {
            let mut position = 0;
            text.chars()
                .filter(|character| {
                    let inside = (start..end).contains(&position);
                    position += width(*character) as u64;
                    inside
                })
                .collect()
        }
        _ => text.to_string(),
    };
    let text = decode_html_entities(&text).trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn media_from_details(media: &Value) -> Option<Media> {
    let info = &media["original_info"];
    let (width, height) = (dimension(&info["width"]), dimension(&info["height"]));
    let kind = media["type"].as_str()?;
    if kind == "photo" {
        let url = image_url(media["media_url_https"].as_str()?)?;
        return Some(Media {
            kind: MediaKind::Image,
            url,
            width,
            height,
            alt: None,
        });
    }

    let url = best_mp4(&media["video_info"]["variants"])?;
    match kind {
        "video" => Some(Media {
            kind: MediaKind::Video,
            url,
            width,
            height,
            alt: None,
        }),
        "animated_gif" => Some(Media {
            kind: MediaKind::Image,
            url: gif_url(&url)?,
            width,
            height,
            alt: None,
        }),
        _ => None,
    }
}

fn best_mp4(variants: &Value) -> Option<String> {
    variants
        .as_array()?
        .iter()
        .filter(|variant| variant["content_type"] == "video/mp4")
        .filter_map(|variant| {
            let url = video_url(variant["url"].as_str()?)?;
            Some((count(&variant["bitrate"]).unwrap_or(0), url))
        })
        .max_by_key(|(bitrate, _)| *bitrate)
        .map(|(_, url)| url)
}

fn gif_url(video_url: &str) -> Option<String> {
    let path = Url::parse(video_url).ok()?.path().to_string();
    let path = path
        .strip_suffix(".mp4")
        .map_or(path.clone(), |stem| format!("{stem}.webp"));
    Some(format!("{GIF_SERVICE_URL}{path}"))
}

pub(super) fn card_vmap_url(card: &Value) -> Option<String> {
    video_url(card["binding_values"]["player_stream_url"]["string_value"].as_str()?)
        .filter(|url| url.ends_with(".vmap"))
}

pub(super) fn card_media(card: &Value, vmap: &str) -> Option<Media> {
    let document = roxmltree::Document::parse(vmap).ok()?;
    let (_, url) = document
        .descendants()
        .filter(|node| {
            node.has_tag_name("videoVariant") && node.attribute("content_type") == Some("video/mp4")
        })
        .filter_map(|node| {
            let url = percent_decode_str(node.attribute("url")?).decode_utf8_lossy();
            let bit_rate = node
                .attribute("bit_rate")
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0);
            Some((bit_rate, video_url(&url)?))
        })
        .max_by_key(|(bit_rate, _)| *bit_rate)?;
    let image = &card["binding_values"]["player_image_original"]["image_value"];

    Some(Media {
        kind: MediaKind::Video,
        url,
        width: dimension(&image["width"]),
        height: dimension(&image["height"]),
        alt: None,
    })
}

pub(super) fn text_from_page(html: &str) -> Option<String> {
    let selector = Selector::parse("h1.sr-only").unwrap();
    let heading: String = Html::parse_document(html)
        .select(&selector)
        .next()?
        .text()
        .collect();
    let (_, text) = heading.trim().split_once(" on X: \"")?;
    let text = text.strip_suffix('"')?.trim();
    (!text.is_empty()).then(|| text.to_string())
}

pub(super) fn decode_html_entities(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&#38;", "&")
        .replace("&amp;", "&")
}

fn image_url(url: &str) -> Option<String> {
    crate::sources::cdn_url(url, &["pbs.twimg.com"])
}

fn video_url(url: &str) -> Option<String> {
    crate::sources::cdn_url(url, &["video.twimg.com"])
}

fn dimension(value: &Value) -> Option<u32> {
    count(value).and_then(|value| u32::try_from(value).ok())
}

fn count(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}

pub(super) fn non_empty_str(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use crate::sources::author_name;
    use axum::http::StatusCode;
    use serde_json::json;

    #[test]
    fn passes_gated_statuses_through_and_rejects_others() {
        let source = source();
        assert_eq!(source.tweet_json("{}").unwrap(), json!({}));
        assert_eq!(
            source
                .tweet_json(r#"{"__typename":"TweetTombstone"}"#)
                .unwrap()["__typename"],
            "TweetTombstone"
        );
        assert_eq!(
            source.tweet_json("not json").unwrap_err().0,
            StatusCode::BAD_GATEWAY
        );
        let other = tweet(
            "2103786785906344020",
            "Wrong",
            "wrong_author",
            "Wrong post",
            [0, 10],
        );
        assert_eq!(
            source.tweet_json(&other.to_string()).unwrap_err().0,
            StatusCode::BAD_GATEWAY
        );
        assert!(source
            .tweet_json(&tweet(STATUS_ID, "Right", "right", "Right post", [0, 10]).to_string())
            .is_ok());
    }

    #[test]
    fn cuts_status_text_with_emoji_at_its_utf16_range() {
        let tweet = tweet(
            STATUS_ID,
            "Entity Author",
            "EliasLou_st",
            "🙂🖌 text https://t.co/photo",
            [0, 9],
        );

        assert_eq!(
            post_from_tweet(&tweet).unwrap().description.as_deref(),
            Some("🙂🖌 text")
        );
    }

    #[test]
    fn extracts_photo_status_with_decoded_text() {
        let tweet = tweet_with_media(
            STATUS_ID,
            "Entity Author",
            "EliasLou_st",
            "&gt;:3c &amp;lt; https://t.co/photo",
            [0, 16],
            photo(IMAGE_URL, 2160, 3840),
        );
        let post = post_from_tweet(&tweet).unwrap();
        let author = post.author.as_ref().unwrap();
        let stats = post.stats.as_ref().unwrap();

        assert_eq!(post.description.as_deref(), Some(">:3c &lt;"));
        assert_eq!(
            post.source_url,
            format!("https://x.com/EliasLou_st/status/{STATUS_ID}")
        );
        assert_eq!(author_name(author), "Entity Author (@EliasLou_st)");
        assert_eq!(
            author.avatar_url.as_deref(),
            Some(&*format!(
                "https://pbs.twimg.com/profile_images/{STATUS_ID}/avatar_400x400.jpg"
            ))
        );
        assert_eq!(
            (stats.likes, stats.shares, stats.comments),
            (Some(12), Some(4), Some(3))
        );
        assert_eq!(post.media.len(), 1);
        assert_eq!(post.media[0].url, IMAGE_URL);
        assert_eq!(
            (post.media[0].width, post.media[0].height),
            (Some(2160), Some(3840))
        );
    }

    #[test]
    fn extracts_graphql_statuses_with_their_quote() {
        let post = post_from_graphql(&graphql_status()).unwrap();
        let author = post.author.as_ref().unwrap();
        let stats = post.stats.as_ref().unwrap();
        let quote = post.quote.as_ref().unwrap();

        assert_eq!(post.provider_id, STATUS_ID);
        assert_eq!(post.description.as_deref(), Some("Four what? >:3"));
        assert_eq!(post.lang.as_deref(), Some("en"));
        assert_eq!(author.username, "MocaRelaxSpace");
        assert_eq!(author.display_name.as_deref(), Some("Moca Space"));
        assert_eq!(
            author.avatar_url.as_deref(),
            Some("https://pbs.twimg.com/profile_images/1/avatar_400x400.jpg")
        );
        assert_eq!(author.verified, Some(true));
        assert_eq!(
            (stats.views, stats.likes, stats.comments, stats.shares),
            (Some(1200), Some(12), Some(3), Some(4))
        );
        assert_eq!(post.media[0].url, IMAGE_URL);
        assert_eq!(quote.provider_id, QUOTE_ID);
        assert_eq!(quote.description.as_deref(), Some("A long note"));
        assert_eq!(quote.media[0].url, HIGH_VIDEO_URL);
        assert!(post_from_graphql(&json!({ "__typename": "TweetTombstone" })).is_none());
    }

    #[test]
    fn dates_statuses_from_their_snowflake_id() {
        assert_eq!(created_at(STATUS_ID), Some(1_790_416_830));
        assert_eq!(created_at("1585341984679469056"), Some(1_666_809_958));
        assert_eq!(created_at("20"), None);
    }

    #[test]
    fn picks_the_highest_bitrate_twimg_mp4() {
        let video = media_from_details(&video("video", HIGH_VIDEO_URL)).unwrap();
        assert_eq!(video.kind, MediaKind::Video);
        assert_eq!(video.url, HIGH_VIDEO_URL);
        assert_eq!((video.width, video.height), (Some(1080), Some(1080)));
    }

    #[test]
    fn serves_gifs_through_the_gif_service() {
        let gif = media_from_details(&video("animated_gif", GIF_VIDEO_URL)).unwrap();
        assert_eq!(gif.kind, MediaKind::Image);
        assert_eq!(gif.url, GIF_URL);
    }

    #[test]
    fn ignores_non_cdn_media() {
        assert!(media_from_details(&photo("https://evil.example.com/photo.jpg", 1, 1)).is_none());
        assert!(image_url("http://pbs.twimg.com/media/photo.jpg").is_none());
        assert!(image_url("https://pbs.twimg.com.evil.example/photo.jpg").is_none());
    }

    #[test]
    fn keeps_the_matching_parent_and_the_quote() {
        let (reply_to, quote) = related_posts(&reply_with_quote());
        assert_eq!(reply_to.unwrap().description.as_deref(), Some("four"));
        assert_eq!(quote.unwrap().media[0].url, HIGH_VIDEO_URL);

        let mut mismatched = reply_with_quote();
        mismatched["in_reply_to_status_id_str"] = json!("1");
        let (reply_to, _) = related_posts(&mismatched);
        assert!(reply_to.is_none());
        assert!(related_posts(&tweet(STATUS_ID, "A", "a", "alone", [0, 5]))
            .0
            .is_none());
    }

    #[test]
    fn media_indexes_run_across_related_posts() {
        let post = reply_with_quote_post();
        let urls: Vec<&str> = post.all_media().map(|media| media.url.as_str()).collect();

        assert_eq!(urls, [IMAGE_URL, IMAGE_TWO, HIGH_VIDEO_URL]);
    }

    #[test]
    fn resolves_card_videos_from_their_vmap() {
        let card = json!({
            "name": "promo_video_convo",
            "binding_values": {
                "player_stream_url": { "string_value": "https://video.twimg.com/amplify_video/vmap/1.vmap", "type": "STRING" },
                "player_image_original": { "image_value": { "url": POSTER_URL, "width": 1280, "height": 720 }, "type": "IMAGE" },
            },
        });
        let vmap = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><vmap:VMAP xmlns:vmap="http://www.iab.net/vmap-1.0" xmlns:tw="http://twitter.com/schema/videoVMapV2.xsd" version="1.0"><tw:videoVariants><tw:videoVariant url="{}" content_type="video/mp4" bit_rate="288000"/><tw:videoVariant url="{}" content_type="video/mp4" bit_rate="2176000"/><tw:videoVariant url="https%3A%2F%2Fvideo.twimg.com%2Fpl.m3u8" content_type="application/x-mpegURL"/></tw:videoVariants></vmap:VMAP>"#,
            "https%3A%2F%2Fvideo.twimg.com%2Fext_tw_video%2Flow.mp4",
            "https%3A%2F%2Fvideo.twimg.com%2Fext_tw_video%2Fhigh.mp4%3Ftag%3D14",
        );

        assert_eq!(
            card_vmap_url(&card).as_deref(),
            Some("https://video.twimg.com/amplify_video/vmap/1.vmap")
        );
        let media = card_media(&card, &vmap).unwrap();
        assert_eq!(media.url, format!("{HIGH_VIDEO_URL}?tag=14"));
        assert_eq!((media.width, media.height), (Some(1280), Some(720)));
        assert!(card_vmap_url(&json!({})).is_none());
    }

    #[test]
    fn reads_full_note_text_from_the_page() {
        let html = r#"<html><h1 class="sr-only">Satya Nadella on X: &quot;Long note with the complete public text &amp; details.&quot;</h1></html>"#;
        assert_eq!(
            text_from_page(html).as_deref(),
            Some("Long note with the complete public text & details.")
        );
        assert!(text_from_page("<html><h1 class=\"sr-only\">Nothing here</h1></html>").is_none());
    }
}
