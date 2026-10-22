use super::FacebookSource;
use serde_json::{json, Value};

pub const POST_CODE: &str = "1DL7Uac7xc";
pub const REEL_CODE: &str = "1MbWc8LmKs";
pub const SHARE_CODE: &str = "1DTvc3LK3k";
pub const IMAGE_ONE: &str = "https://scontent-lhr8-1.xx.fbcdn.net/post-one.jpg";
pub const IMAGE_TWO: &str = "https://scontent-lhr8-2.xx.fbcdn.net/post-two.jpg";
pub const IMAGE_THREE: &str = "https://scontent-lhr8-3.xx.fbcdn.net/post-three.jpg";
pub const VIDEO_URL: &str = "https://video-lhr8-1.xx.fbcdn.net/reel.mp4?a=1&b=2";
pub const VIDEO_ID: &str = "1551678869998267";

pub fn source(url: &str) -> FacebookSource {
    FacebookSource::from_url(url).expect(url)
}

pub fn post() -> FacebookSource {
    source(&format!("https://www.facebook.com/share/p/{POST_CODE}/"))
}

pub fn reel() -> FacebookSource {
    source(&format!("https://www.facebook.com/share/r/{REEL_CODE}/"))
}

pub fn relay(data: Value) -> String {
    let payload =
        json!({ "require": [["ScheduledServerJS", { "__bbox": { "result": { "data": data } } }]] });
    format!(r#"<script type="application/json" data-sjs>{payload}</script>"#)
}

pub fn photo(id: &str, url: &str) -> Value {
    json!({
        "__typename": "Photo",
        "id": id,
        "image": { "uri": "https://scontent.xx.fbcdn.net/small.jpg" },
        "viewer_image": { "uri": url },
    })
}

pub fn image_post_html(image_count: usize) -> String {
    let images: Vec<Value> = [IMAGE_ONE, IMAGE_TWO, IMAGE_THREE][..image_count]
        .iter()
        .enumerate()
        .map(|(index, url)| json!({ "media": photo(&index.to_string(), url) }))
        .collect();
    let post = json!({ "node_v2": { "attachments": [{ "styles": { "attachment": {
        "all_subattachments": { "nodes": images },
    } } }] } });
    let related = json!({ "edges": [{ "node": { "media": photo("9", "https://scontent.xx.fbcdn.net/related.jpg") } }] });
    format!(
        r#"<!doctype html><html><head>
        <meta property="og:title" content="Facebook author">
        <meta property="og:description" content="Facebook post">
        </head><body>{}{}</body></html>"#,
        relay(post),
        relay(related),
    )
}

pub fn reel_html() -> String {
    let video = json!({ "video": {
        "id": VIDEO_ID,
        "width": 1080,
        "height": 1920,
        "videoDeliveryLegacyFields": { "browser_native_hd_url": VIDEO_URL },
    } });
    format!(
        r#"<!doctype html><html><head>
        <meta property="og:title" content="Facebook reel">
        <meta property="og:url" content="https://www.facebook.com/reel/{VIDEO_ID}/">
        </head><body>{}</body></html>"#,
        relay(video)
    )
}
