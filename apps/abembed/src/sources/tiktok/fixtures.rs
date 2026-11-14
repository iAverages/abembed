use super::TikTokSource;
use serde_json::{json, Value};

pub const USERNAME: &str = "kopilawak";
pub const VIDEO_ID: &str = "7665179028352945426";
pub const PHOTO_ID: &str = "7666981398800108813";
pub const WEBAPP_VIDEO_URL: &str =
    "https://v16-webapp-prime.tiktok.com/video/tos/no1a/video/?mime_type=video_mp4";
pub const AWEME_PLAY_URL: &str = "https://www.tiktok.com/aweme/v1/play/?faid=1988&file_id=37a4&is_play_url=1&item_id=7665179028352945426&video_id=v24044";
pub const DASH_PLAY_URL: &str = "https://www.tiktok.com/aweme/v1/play/?faid=1988&file_id=d2f2&is_play_url=1&item_id=7665179028352945426";
pub const DASH_AUDIO_PLAY_URL: &str = "https://www.tiktok.com/aweme/v1/play/?faid=1988&file_id=2119&is_play_url=1&item_id=7665179028352945426";
pub const EMBED_VIDEO_URL: &str = "https://v45.tiktokcdn-eu.com/video.mp4?mime_type=video_mp4";
pub const IMAGE_ONE: &str =
    "https://p16-common-sign.tiktokcdn-eu.com/first.jpeg?x-expires=1785416400";
pub const IMAGE_TWO: &str = "https://p16-common-sign.tiktokcdn-eu.com/second.jpeg";
pub const AVATAR_URL: &str = "https://p16.tiktokcdn.com/avatar.jpeg";

pub fn source(post_type: &str, id: &str) -> TikTokSource {
    TikTokSource::new(USERNAME, post_type, id).unwrap()
}

pub fn script(id: &str, value: Value) -> String {
    format!(
        r#"<!doctype html><html><body><script id="{id}" type="application/json">{value}</script></body></html>"#
    )
}

pub fn video_page() -> String {
    video_page_with_audio(json!([
        { "Bitrate": 33726, "UrlList": { "FallbackUrl": "https://www.tiktok.com/aweme/v1/play/?faid=1988&file_id=b327&item_id=7665179028352945426" } },
        { "Bitrate": 49656, "UrlList": { "FallbackUrl": DASH_AUDIO_PLAY_URL } },
    ]))
}

pub fn video_page_without_audio() -> String {
    video_page_with_audio(Value::Null)
}

fn video_page_with_audio(audio: Value) -> String {
    script(
        "__UNIVERSAL_DATA_FOR_REHYDRATION__",
        json!({ "__DEFAULT_SCOPE__": { "webapp.video-detail": { "itemInfo": { "itemStruct": {
            "id": VIDEO_ID,
            "desc": "Coffee time",
            "createTime": "1784688572",
            "author": { "uniqueId": USERNAME, "nickname": "Kopi Lawak", "avatarThumb": AVATAR_URL },
            "stats": { "diggCount": 21, "shareCount": 5, "commentCount": 8, "playCount": 100 },
            "video": {
                "width": 720,
                "height": 1280,
                "duration": 22,
                "playAddr": WEBAPP_VIDEO_URL,
                "bitrateInfo": [
                    { "Format": "mp4", "Bitrate": 409572, "PlayAddr": { "Width": 720, "Height": 1280, "UrlList": [WEBAPP_VIDEO_URL, AWEME_PLAY_URL] } },
                    { "Format": "dash", "Bitrate": 708911, "PlayAddr": { "Width": 1080, "Height": 1920, "UrlList": [WEBAPP_VIDEO_URL, DASH_PLAY_URL] } },
                ],
                "bitrateAudioInfo": audio,
            },
        } } } } }),
    )
}

pub fn embed_page(id: &str, video_data: Value) -> String {
    script(
        "__FRONTITY_CONNECT_STATE__",
        json!({ "source": { "data": { format!("/embed/v2/{id}"): { "videoData": video_data } } } }),
    )
}

pub fn photo_embed_page() -> String {
    embed_page(
        PHOTO_ID,
        json!({
            "authorInfos": { "uniqueId": USERNAME, "nickName": "Photo Author", "covers": [AVATAR_URL] },
            "itemInfos": {
                "id": PHOTO_ID,
                "text": "Photo description",
                "diggCount": 31,
                "shareCount": 6,
                "commentCount": "9",
                "covers": [IMAGE_ONE],
            },
            "imagePostInfo": { "displayImages": [
                { "urlList": [IMAGE_ONE], "width": 814, "height": 376 },
                { "urlList": [IMAGE_TWO], "width": 640, "height": 640 },
            ] },
        }),
    )
}
