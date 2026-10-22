use super::InstagramSource;
use crate::sources::EmbedPost;
use serde_json::{json, Value};

pub const SHORTCODE: &str = "AbC_123";
pub const IMAGE_ONE: &str = "https://cdninstagram.com/image-one.jpg";
pub const IMAGE_TWO: &str = "https://cdninstagram.com/image-two.jpg";
pub const VIDEO_URL: &str = "https://video.cdninstagram.com/video.mp4";
pub const AVATAR_URL: &str = "https://cdninstagram.com/avatar.jpg";

pub fn html_with_scripts(values: &[Value]) -> String {
    let scripts: String = values
        .iter()
        .map(|value| format!("<script>{value}</script>"))
        .collect();
    format!("<!doctype html><html><head>{scripts}</head></html>")
}

pub fn image_node(url: &str) -> Value {
    json!({ "image_versions2": { "candidates": [{ "url": url, "width": 640, "height": 640 }] } })
}

pub fn carousel_html() -> String {
    html_with_scripts(&[
        json!({ "post": { "shortcode": SHORTCODE } }),
        json!({ "post": {
            "caption": { "text": "Instagram description" },
            "carousel_media": [image_node(IMAGE_ONE), image_node(IMAGE_TWO)],
            "shortcode": SHORTCODE,
            "comment_count": 4,
            "like_count": 12,
            "reshare_count": 3,
            "user": { "full_name": "Poster Name", "profile_pic_url": AVATAR_URL, "username": "poster" },
        } }),
    ])
}

pub fn source(media_type: &str) -> InstagramSource {
    InstagramSource::new(media_type, SHORTCODE).unwrap()
}

pub fn media_urls(embed: &EmbedPost) -> Vec<&str> {
    embed.media.iter().map(|media| media.url.as_str()).collect()
}

pub fn gated_page() -> String {
    html_with_scripts(&[json!({ "xig_polaris_media": {
        "code": SHORTCODE,
        "gating_ruling": { "title": "Age-restricted content" },
    } })])
}

pub fn unavailable_page() -> String {
    format!(
        r#"<script>{{"route":{{"props":{{"page_logging":{{"name":"httpErrorPage","params":{{}}}},"failure_reason":null,"restricted_age":null,"page_type":"MEDIA"}}}},"polarisRouteConfig":{{"pageID":"httpErrorPage"}},"url":"\/p\/{SHORTCODE}\/"}}</script>"#
    )
}
