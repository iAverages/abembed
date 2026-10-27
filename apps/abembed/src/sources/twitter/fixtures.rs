use super::page::{post_from_tweet, related_posts};
use super::TwitterSource;
use crate::sources::EmbedPost;
use serde_json::{json, Value};

pub const STATUS_ID: &str = "2103786785906344019";
pub const PARENT_ID: &str = "2103649287028736479";
pub const QUOTE_ID: &str = "2102947640124244226";
pub const IMAGE_URL: &str = "https://pbs.twimg.com/media/photo-one.jpg";
pub const IMAGE_TWO: &str = "https://pbs.twimg.com/media/photo-two.jpg";
pub const POSTER_URL: &str = "https://pbs.twimg.com/ext_tw_video_thumb/poster.jpg";
pub const LOW_VIDEO_URL: &str = "https://video.twimg.com/ext_tw_video/low.mp4";
pub const HIGH_VIDEO_URL: &str = "https://video.twimg.com/ext_tw_video/high.mp4";
pub const GIF_VIDEO_URL: &str = "https://video.twimg.com/tweet_video/HTNOycZXIAAn9i8.mp4";
pub const GIF_URL: &str = "https://gif.abembed.com/tweet_video/HTNOycZXIAAn9i8.webp";
pub const BASE: &str = "https://abembed.com/twitter";

pub fn source() -> TwitterSource {
    TwitterSource::new("EliasLou_st", STATUS_ID, None).unwrap()
}

pub fn tweet(id: &str, name: &str, username: &str, text: &str, range: [usize; 2]) -> Value {
    json!({
        "__typename": "Tweet",
        "id_str": id,
        "text": text,
        "display_text_range": range,
        "favorite_count": 12,
        "conversation_count": 3,
        "retweet_count": 4,
        "user": {
            "name": name,
            "screen_name": username,
            "profile_image_url_https": format!("https://pbs.twimg.com/profile_images/{id}/avatar_normal.jpg"),
        },
        "mediaDetails": [],
    })
}

pub fn tweet_with_media(
    id: &str,
    name: &str,
    username: &str,
    text: &str,
    range: [usize; 2],
    media: Value,
) -> Value {
    let mut tweet = tweet(id, name, username, text, range);
    tweet["mediaDetails"] = json!([media]);
    tweet
}

pub fn photo(url: &str, width: u32, height: u32) -> Value {
    json!({
        "type": "photo",
        "media_url_https": url,
        "original_info": { "width": width, "height": height },
    })
}

pub fn video(kind: &str, high_url: &str) -> Value {
    json!({
        "type": kind,
        "media_url_https": POSTER_URL,
        "original_info": { "width": 1080, "height": 1080 },
        "video_info": { "variants": [
            { "content_type": "application/x-mpegURL", "url": "https://video.twimg.com/video.m3u8" },
            { "bitrate": 256000, "content_type": "video/mp4", "url": LOW_VIDEO_URL },
            { "bitrate": 2176000, "content_type": "video/mp4", "url": high_url },
            { "bitrate": 9000000, "content_type": "video/mp4", "url": "https://evil.example.com/high.mp4" },
        ] },
    })
}

// a photo reply to a photo post, quoting a video post
pub fn reply_with_quote() -> Value {
    let mut tweet = tweet(
        STATUS_ID,
        "Moca Space",
        "MocaRelaxSpace",
        "@butchtimoris Four what?",
        [14, 24],
    );
    tweet["mediaDetails"] = json!([photo(IMAGE_URL, 2160, 3840)]);
    tweet["in_reply_to_status_id_str"] = json!(PARENT_ID);
    tweet["parent"] = tweet_with_media(
        PARENT_ID,
        "mari",
        "butchtimoris",
        "four https://t.co/photo",
        [0, 4],
        photo(IMAGE_TWO, 618, 848),
    );
    tweet["quoted_tweet"] = tweet_with_media(
        QUOTE_ID,
        "Food Lover",
        "_FoodieX",
        "Warm cookies",
        [0, 12],
        video("video", HIGH_VIDEO_URL),
    );
    tweet
}

pub fn reply_with_quote_post() -> EmbedPost {
    let tweet = reply_with_quote();
    let mut post = post_from_tweet(&tweet).unwrap();
    let (reply_to, quote) = related_posts(&tweet);
    post.reply_to = reply_to.map(Box::new);
    post.quote = quote.map(Box::new);
    post
}

// `tweetResult.result` of the graphql status query, a blue-verified reply quoting a note tweet
pub fn graphql_status() -> Value {
    json!({
        "__typename": "TweetWithVisibilityResults",
        "tweet": {
            "rest_id": STATUS_ID,
            "core": { "user_results": { "result": {
                "is_blue_verified": true,
                "core": { "name": "Moca Space", "screen_name": "MocaRelaxSpace" },
                "avatar": { "image_url": "https://pbs.twimg.com/profile_images/1/avatar_normal.jpg" },
            } } },
            "views": { "count": "1200" },
            "legacy": {
                "id_str": STATUS_ID,
                "full_text": "@butchtimoris Four what? &gt;:3 https://t.co/photo",
                "display_text_range": [14, 31],
                "lang": "en",
                "favorite_count": 12,
                "reply_count": 3,
                "retweet_count": 4,
                "extended_entities": { "media": [photo(IMAGE_URL, 2160, 3840)] },
            },
            "quoted_status_result": { "result": {
                "__typename": "Tweet",
                "rest_id": QUOTE_ID,
                "core": { "user_results": { "result": {
                    "legacy": { "name": "Food Lover", "screen_name": "_FoodieX", "profile_image_url_https": "https://pbs.twimg.com/profile_images/2/avatar_normal.jpg" },
                } } },
                "note_tweet": { "note_tweet_results": { "result": { "text": "A long note" } } },
                "legacy": {
                    "id_str": QUOTE_ID,
                    "full_text": "A long…",
                    "lang": "en",
                    "extended_entities": { "media": [video("video", HIGH_VIDEO_URL)] },
                },
            } },
        },
    })
}

// the discord component json embedded in an embed page
pub fn component(body: &str) -> Value {
    let start =
        body.find(r#"type="application/json">"#).unwrap() + r#"type="application/json">"#.len();
    let end = body.find("</script>").unwrap();
    serde_json::from_str::<Value>(&body[start..end]).unwrap()["component"].take()
}
