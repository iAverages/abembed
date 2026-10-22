// parsing of the instagram pages the provider workers fetch

use super::InstagramSource;
use crate::sources::{
    bad_gateway, objects, Author, EmbedPost, HttpError, Media, MediaKind, Source, Stats,
};
use scraper::{Html, Selector};
use serde_json::{Map, Value};

impl InstagramSource {
    pub(super) fn embed_from_html(&self, html: &str) -> Result<EmbedPost, HttpError> {
        let dom = Html::parse_document(html);
        let selector = Selector::parse("script").unwrap();
        let scripts: Vec<String> = dom
            .select(&selector)
            .map(|script| script.text().collect())
            .collect();

        let metadata = extract_metadata(&scripts, &self.shortcode);
        let (media, title, description) = match extract_media(&scripts, &self.shortcode) {
            Some(media) => (
                media
                    .into_iter()
                    .map(validate_media)
                    .collect::<Result<Vec<_>, _>>()?,
                None,
                metadata.description,
            ),
            None => {
                let (title, description) = extract_gating(&scripts, &self.shortcode)
                    .or_else(|| {
                        is_unavailable_page(html).then(|| {
                            (
                                "Post isn't available".to_string(),
                                Some(
                                    "The link may be broken, or the post may have been removed."
                                        .to_string(),
                                ),
                            )
                        })
                    })
                    .ok_or_else(|| {
                        bad_gateway("Instagram page did not include a direct media URL")
                    })?;
                (Vec::new(), Some(title), description)
            }
        };

        Ok(EmbedPost {
            provider: "instagram".to_string(),
            provider_id: self.shortcode.clone(),
            source_url: self.source_url(),
            title,
            description,
            author: metadata.author,
            media,
            stats: Some(metadata.stats),
        })
    }
}

#[derive(Default)]
struct Metadata {
    author: Option<Author>,
    description: Option<String>,
    stats: Stats,
}

fn extract_metadata(scripts: &[String], shortcode: &str) -> Metadata {
    let mut metadata = Metadata::default();

    for script in scripts.iter().filter(|script| script.contains(shortcode)) {
        if let Ok(json) = serde_json::from_str::<Value>(script.trim()) {
            for record in objects(&json).filter(|record| matches_shortcode(record, shortcode)) {
                apply_record_metadata(record, &mut metadata);
            }
        }
    }

    metadata
}

fn apply_record_metadata(record: &Map<String, Value>, metadata: &mut Metadata) {
    let count = |key: &str| record.get(key)?.get("count")?.as_u64();
    let caption = record
        .get("caption")
        .and_then(|caption| non_empty_str(caption.get("text")))
        .or_else(|| {
            non_empty_str(
                record
                    .get("edge_media_to_caption")
                    .and_then(|edge| edge.pointer("/edges/0/node/text")),
            )
        })
        .or_else(|| non_empty_str(record.get("accessibility_caption")));
    let author = ["user", "owner"]
        .iter()
        .find_map(|key| record.get(*key)?.as_object().and_then(author_from_record))
        .or_else(|| author_from_record(record));

    let stats = &mut metadata.stats;
    let likes = number(record, "like_count")
        .or_else(|| count("edge_media_preview_like"))
        .or_else(|| count("edge_liked_by"));
    let shares = number(record, "reshare_count").or_else(|| number(record, "share_count"));
    let comments = number(record, "comment_count")
        .or_else(|| count("edge_media_to_comment"))
        .or_else(|| count("edge_media_to_parent_comment"));

    stats.likes = likes.or(stats.likes);
    stats.shares = shares.or(stats.shares);
    stats.comments = comments.or(stats.comments);
    metadata.author = author.or(metadata.author.take());
    metadata.description = caption
        .map(|desc| desc.to_string())
        .or(metadata.description.take());
}

fn author_from_record(record: &Map<String, Value>) -> Option<Author> {
    let username = non_empty_str(record.get("username"))?;
    let display_name = ["full_name", "fullName", "name"]
        .iter()
        .find_map(|key| non_empty_str(record.get(*key)));
    let avatar_url = ["profile_pic_url_hd", "profile_pic_url"]
        .iter()
        .find_map(|key| non_empty_str(record.get(*key)));

    Some(Author {
        id: None,
        username: username.to_string(),
        display_name: display_name.map(|name| name.to_string()),
        profile_url: Some(format!("https://www.instagram.com/{username}/")),
        avatar_url: avatar_url.map(|url| url.to_string()),
        verified: None,
    })
}

fn extract_media(scripts: &[String], shortcode: &str) -> Option<Vec<Media>> {
    find_in_scripts(
        scripts,
        &["carousel_media", "edge_sidecar_to_children"],
        |record| carousel_from_record(record, shortcode),
    )
    .or_else(|| {
        find_in_scripts(scripts, &["video_versions"], video_from_versions).map(|media| vec![media])
    })
    .or_else(|| {
        find_in_scripts(scripts, &["image_versions2"], image_from_candidates)
            .map(|media| vec![media])
    })
}

fn is_unavailable_page(html: &str) -> bool {
    html.contains(r#""pageID":"httpErrorPage""#) && html.contains(r#""page_type":"MEDIA""#)
}

fn extract_gating(scripts: &[String], shortcode: &str) -> Option<(String, Option<String>)> {
    find_in_scripts(scripts, &["gating_ruling"], |record| {
        if !matches_shortcode(record, shortcode) {
            return None;
        }
        let ruling = record.get("gating_ruling")?;
        Some((
            non_empty_str(ruling.get("title"))?.to_string(),
            non_empty_str(ruling.get("description")).map(|desc| desc.to_string()),
        ))
    })
}

fn find_in_scripts<T>(
    scripts: &[String],
    keys: &[&str],
    found: impl Fn(&Map<String, Value>) -> Option<T>,
) -> Option<T> {
    scripts
        .iter()
        .filter(|script| keys.iter().any(|key| script.contains(key)))
        .filter_map(|script| serde_json::from_str::<Value>(script.trim()).ok())
        .find_map(|json| objects(&json).find_map(&found))
}

fn carousel_from_record(record: &Map<String, Value>, shortcode: &str) -> Option<Vec<Media>> {
    if !matches_shortcode(record, shortcode) {
        return None;
    }

    record
        .get("carousel_media")
        .and_then(carousel_items)
        .filter(|media| !media.is_empty())
        .or_else(|| {
            record
                .get("edge_sidecar_to_children")
                .and_then(|sidecar| sidecar_items(sidecar.get("edges")?))
                .filter(|media| !media.is_empty())
        })
}

fn carousel_items(items: &Value) -> Option<Vec<Media>> {
    Some(
        items
            .as_array()?
            .iter()
            .filter_map(|item| media_from_record(item.as_object()?))
            .collect(),
    )
}

fn sidecar_items(edges: &Value) -> Option<Vec<Media>> {
    Some(
        edges
            .as_array()?
            .iter()
            .filter_map(|edge| media_from_record(edge.get("node")?.as_object()?))
            .collect(),
    )
}

fn matches_shortcode(record: &Map<String, Value>, shortcode: &str) -> bool {
    ["shortcode", "code"]
        .iter()
        .any(|key| record.get(*key).and_then(|key| key.as_str()) == Some(shortcode))
}

fn media_from_record(record: &Map<String, Value>) -> Option<Media> {
    video_from_versions(record)
        .or_else(|| url_media(record, "video_url", MediaKind::Video))
        .or_else(|| image_from_candidates(record))
        .or_else(|| url_media(record, "display_url", MediaKind::Image))
        .or_else(|| url_media(record, "display_src", MediaKind::Image))
}

fn video_from_versions(record: &Map<String, Value>) -> Option<Media> {
    first_with_url(record.get("video_versions")?, record, MediaKind::Video)
}

fn image_from_candidates(record: &Map<String, Value>) -> Option<Media> {
    first_with_url(
        record.get("image_versions2")?.get("candidates")?,
        record,
        MediaKind::Image,
    )
}

fn first_with_url(items: &Value, record: &Map<String, Value>, kind: MediaKind) -> Option<Media> {
    items.as_array()?.iter().find_map(|item| {
        let candidate = item.as_object()?;
        let url = candidate.get("url")?.as_str()?;
        Some(new_media(kind, url, record, Some(candidate)))
    })
}

fn url_media(record: &Map<String, Value>, key: &str, kind: MediaKind) -> Option<Media> {
    let url = record.get(key)?.as_str()?;
    Some(new_media(kind, url, record, None))
}

fn new_media(
    kind: MediaKind,
    url: &str,
    record: &Map<String, Value>,
    candidate: Option<&Map<String, Value>>,
) -> Media {
    let dimension = |candidate_key: &str, original_key: &str| {
        candidate
            .and_then(|candidate| number(candidate, candidate_key))
            .or_else(|| number(record, original_key))
            .or_else(|| record.get("dimensions")?.get(candidate_key)?.as_u64())
            .and_then(|value| u32::try_from(value).ok())
    };

    Media {
        kind,
        url: url.to_string(),
        width: dimension("width", "original_width"),
        height: dimension("height", "original_height"),
    }
}

fn number(record: &Map<String, Value>, key: &str) -> Option<u64> {
    record.get(key)?.as_u64()
}

fn non_empty_str(value: Option<&Value>) -> Option<&str> {
    value?.as_str().filter(|value| !value.is_empty())
}

fn validate_media(mut media: Media) -> Result<Media, HttpError> {
    let decoded = media
        .url
        .replace("\\/", "/")
        .replace("\\u0026", "&")
        .replace("&amp;", "&")
        .replace("&#38;", "&");
    media.url = crate::sources::cdn_url(&decoded, &["fbcdn.net", "cdninstagram.com"])
        .ok_or_else(|| bad_gateway("Extracted media URL was not an Instagram CDN URL"))?;
    Ok(media)
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use crate::sources::author_name;
    use axum::http::StatusCode;
    use serde_json::json;

    #[test]
    fn extracts_carousel_with_metadata() {
        let embed = source("p").embed_from_html(&carousel_html()).unwrap();
        let author = embed.author.as_ref().unwrap();
        let stats = embed.stats.as_ref().unwrap();

        assert_eq!(media_urls(&embed), [IMAGE_ONE, IMAGE_TWO]);
        assert_eq!(embed.media[0].width, Some(640));
        assert_eq!(embed.description.as_deref(), Some("Instagram description"));
        assert_eq!(author_name(author), "Poster Name (@poster)");
        assert_eq!(author.avatar_url.as_deref(), Some(AVATAR_URL));
        assert_eq!(
            (stats.likes, stats.shares, stats.comments),
            (Some(12), Some(3), Some(4))
        );
        assert_eq!(
            embed.source_url,
            format!("https://www.instagram.com/p/{SHORTCODE}/")
        );
    }

    #[test]
    fn ignores_unrelated_carousels_when_extracting_a_reel() {
        let html = html_with_scripts(&[json!({
            "related": { "carousel_media": [image_node(IMAGE_ONE)], "shortcode": "Unrelated" },
            "target": {
                "image_versions2": image_node(IMAGE_TWO)["image_versions2"],
                "shortcode": SHORTCODE,
                "video_versions": [{ "height": 1920, "url": VIDEO_URL, "width": 1080 }],
            },
        })]);
        let embed = source("reels").embed_from_html(&html).unwrap();

        assert_eq!(media_urls(&embed), [VIDEO_URL]);
        assert_eq!(embed.media[0].kind, MediaKind::Video);
    }

    #[test]
    fn treats_unavailable_pages_like_gated_posts() {
        let embed = source("p").embed_from_html(&unavailable_page()).unwrap();

        assert!(embed.media.is_empty());
        assert_eq!(embed.title.as_deref(), Some("Post isn't available"));
    }

    #[test]
    fn errors_on_pages_with_nothing_recognisable() {
        assert!(source("p").embed_from_html("<html></html>").is_err());
    }

    #[test]
    fn rejects_non_cdn_media() {
        let html = html_with_scripts(&[json!({
            "shortcode": SHORTCODE,
            "video_versions": [{ "url": "https://evil.example.com/video.mp4" }],
        })]);
        let HttpError(status, _) = source("reel").embed_from_html(&html).unwrap_err();

        assert_eq!(status, StatusCode::BAD_GATEWAY);
    }
}
