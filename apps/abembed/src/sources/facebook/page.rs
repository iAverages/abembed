// parsing of the facebook pages the provider workers fetch

use super::{is_name, FacebookSource};
use crate::sources::{
    bad_gateway, objects, Author, EmbedPost, HttpError, Media, MediaKind, Source, Stats,
};
use regex::Regex;
use scraper::{Html, Selector};
use serde_json::{Map, Value};
use std::{collections::BTreeMap, sync::LazyLock};
use worker::Url;

static REACTION_COUNT_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""reaction_count":\{"count":(\d+)"#).unwrap());
static SHARE_COUNT_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""share_count":\{"count":(\d+)"#).unwrap());
static COMMENT_COUNT_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""comments":\{"total_count":(\d+)"#).unwrap());
static REEL_REACTION_COUNT_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""unified_reactors":\{"count":(\d+)"#).unwrap());
static REEL_SHARE_COUNT_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""share_count_reduced":"(\d+)""#).unwrap());
static REEL_COMMENT_COUNT_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""total_comment_count":(\d+)"#).unwrap());
static VIEW_COUNT_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""video_view_count":(\d+)"#).unwrap());
static OWNER_ID_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""owning_profile":\{"__typename":"\w+","name":"(?:[^"\\]|\\.)*",(?:"short_name":"(?:[^"\\]|\\.)*",)?"id":"(\w+)""#)
        .unwrap()
});
static REEL_OWNER_ID_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""owner":\{"__typename":"\w+","id":"(\w+)""#).unwrap());
static VIDEO_ID_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:/reel/|/videos/(?:[^/?#]+/)?|[?&](?:v|video_id)=)(\d+)").unwrap()
});
static VIDEO_ID_PARAM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""video_id":"(\d+)""#).unwrap());
const VIDEO_ROUTE_MARKER: &str = r#""entity_id":{"source":"param","value":"video_id"}"#;

const FORMAT_KEYS: [&str; 8] = [
    "videoDeliveryLegacyFields",
    "videoDeliveryResponseFragment",
    "playable_url",
    "playable_url_quality_hd",
    "browser_native_hd_url",
    "browser_native_sd_url",
    "dash_manifest",
    "dash_manifest_xml_string",
];

// merges the separate dash video and audio streams, see apps/proxy
const MERGE_PROXY_URL: &str = "https://proxy.abembed.com/merge";

impl FacebookSource {
    pub(super) fn embed_from_html(&self, html: &str) -> Result<EmbedPost, HttpError> {
        if self.path.starts_with("stories/") {
            return self.story_embed(html);
        }
        let meta = meta_tags(html);

        let video_id = [meta.get("og:url"), Some(&self.source_url())]
            .into_iter()
            .flatten()
            .find_map(|url| Some(VIDEO_ID_PATTERN.captures(url)?[1].to_string()));

        let posts = post_nodes(html);
        let attachments: Vec<&Value> = posts
            .iter()
            .flat_map(|post| {
                let own = post["attachments"]
                    .as_array()
                    .filter(|list| !list.is_empty());
                let shared = || {
                    objects(&post["comet_sections"]["content"])
                        .find_map(|map| map.get("attachments")?.as_array())
                        .filter(|list| !list.is_empty())
                };
                match own.or_else(shared) {
                    Some(attachments) => attachments.iter().collect(),
                    None => vec![post],
                }
            })
            .collect();

        let video_id = video_id
            .or_else(|| {
                attachments
                    .iter()
                    .flat_map(|attachment| objects(attachment))
                    .find(|map| map.get("__typename").and_then(Value::as_str) == Some("Video"))
                    .and_then(|map| map.get("id")?.as_str().map(str::to_string))
            })
            .or_else(|| {
                html.contains(VIDEO_ROUTE_MARKER)
                    .then(|| Some(VIDEO_ID_PARAM.captures(html)?[1].to_string()))
                    .flatten()
            });

        let media = match &video_id {
            Some(video_id) => vec![video(html, video_id)?],
            None => photos(&attachments)?,
        };

        let description = meta.get("og:description").cloned().or_else(|| {
            posts.iter().find_map(|post| {
                post["creation_story"]["message"]["text"]
                    .as_str()
                    .map(str::to_string)
            })
        });
        let (author, stats) =
            post_details(html).map_or((None, None), |(author, stats)| (author, Some(stats)));

        Ok(EmbedPost {
            provider: "facebook".to_string(),
            provider_id: self.id.clone(),
            source_url: self.source_url(),
            title: meta.get("og:title").cloned(),
            description,
            author,
            media,
            stats,
        })
    }

    fn story_embed(&self, html: &str) -> Result<EmbedPost, HttpError> {
        let token = self.path.rsplit('/').next().unwrap_or_default();

        let card = relay_blobs(html)
            .filter(|blob| blob.contains(token))
            .filter_map(|blob| serde_json::from_str::<Value>(blob).ok())
            .find_map(|json| {
                objects(&json)
                    .filter(|map| {
                        map.get("id").and_then(Value::as_str) == Some(token)
                            && map
                                .get("attachments")
                                .and_then(Value::as_array)
                                .is_some_and(|list| !list.is_empty())
                    })
                    .max_by_key(|map| map.len())
                    .map(|map| Value::Object(map.clone()))
            })
            .ok_or_else(|| bad_gateway("Facebook page did not include the story"))?;

        let attachments: Vec<&Value> = card["attachments"]
            .as_array()
            .map(|list| list.iter().collect())
            .unwrap_or_default();

        let video_id = attachments
            .iter()
            .flat_map(|attachment| objects(attachment))
            .find(|map| map.get("__typename").and_then(Value::as_str) == Some("Video"))
            .and_then(|map| map.get("id")?.as_str());

        let media = match video_id {
            Some(video_id) => vec![video(html, video_id)?],
            None => photos(&attachments)?,
        };

        let author = card["actors"][0]["id"].as_str().map(|id| author(html, id));
        let stats = Stats {
            views: None,
            likes: card["story_card_info"]["feedback_summary"]["total_reaction_count"].as_u64(),
            comments: None,
            shares: None,
        };

        Ok(EmbedPost {
            provider: "facebook".to_string(),
            provider_id: self.id.clone(),
            source_url: self.source_url(),
            title: None,
            description: card["message"]["text"].as_str().map(str::to_string),
            author,
            media,
            stats: Some(stats),
        })
    }
}

fn video(html: &str, video_id: &str) -> Result<Media, HttpError> {
    let node = relay_blobs(html)
        .filter(|blob| blob.contains(video_id))
        .filter_map(|blob| serde_json::from_str::<Value>(blob).ok())
        .find_map(|json| Some(Value::Object(find_video(&json, video_id)?.clone())))
        .ok_or_else(|| bad_gateway(format!("Facebook page did not include video {video_id}")))?;
    let representations: Vec<Representation> = manifests(&node)
        .iter()
        .flat_map(|manifest| parse_manifest(manifest))
        .collect();

    if let (Some(video), Some(audio)) = (best_video(&representations), best_audio(&representations))
    {
        return Ok(Media {
            kind: MediaKind::Video,
            url: merged_video_url(&video.url, &audio.url),
            width: Some(video.width).filter(|width| *width > 0),
            height: Some(video.height).filter(|height| *height > 0),
        });
    }

    let legacy = node
        .get("videoDeliveryLegacyFields")
        .filter(|fields| fields.is_object())
        .unwrap_or(&node);
    let url = [
        "browser_native_hd_url",
        "browser_native_sd_url",
        "playable_url_quality_hd",
        "playable_url",
    ]
    .iter()
    .find_map(|key| legacy[*key].as_str().or_else(|| node[*key].as_str()))
    .ok_or_else(|| bad_gateway(format!("Facebook video {video_id} has no playable url")))?;
    let dimension = |key: &str| {
        node[key]
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| *value > 0)
    };

    Ok(Media {
        kind: MediaKind::Video,
        url: cdn_url(url).ok_or_else(not_cdn)?,
        width: dimension("width"),
        height: dimension("height"),
    })
}

fn post_nodes(html: &str) -> Vec<Value> {
    relay_blobs(html)
        .filter(|blob| blob.contains("\"node_v2\"") || blob.contains("\"currMedia\""))
        .filter_map(|blob| serde_json::from_str::<Value>(blob).ok())
        .flat_map(|json| ["node_v2", "currMedia"].map(|key| find_key(&json, key).cloned()))
        .flatten()
        .collect()
}

fn find_key<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    objects(value).find_map(|map| map.get(key).filter(|child| child.is_object()))
}

fn photos(attachments: &[&Value]) -> Result<Vec<Media>, HttpError> {
    let mut photos: Vec<(&str, &str)> = Vec::new();
    for map in attachments
        .iter()
        .flat_map(|attachment| objects(attachment))
    {
        if map.get("__typename").and_then(Value::as_str) != Some("Photo") {
            continue;
        }
        let id = map.get("id").and_then(Value::as_str).unwrap_or_default();

        let url = ["viewer_image", "photo_image", "image"]
            .iter()
            .find_map(|key| map.get(*key)?["uri"].as_str());
        if let Some(url) = url.filter(|_| !photos.iter().any(|(seen, _)| *seen == id)) {
            photos.push((id, url));
        }
    }
    if photos.is_empty() {
        return Err(bad_gateway("Facebook page did not include any media"));
    }

    photos
        .into_iter()
        .map(|(_, url)| cdn_url(url).map(image))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(not_cdn)
}

fn merged_video_url(video: &str, audio: &str) -> String {
    Url::parse_with_params(MERGE_PROXY_URL, [("video", video), ("audio", audio)])
        .unwrap()
        .to_string()
}

fn relay_blobs(html: &str) -> impl Iterator<Item = &str> {
    html.split("data-sjs>")
        .skip(1)
        .filter_map(|rest| rest.find("</script>").map(|end| &rest[..end]))
}

fn post_details(html: &str) -> Result<(Option<Author>, Stats), HttpError> {
    let count = |pattern: &Regex| pattern.captures(html)?[1].parse().ok();
    let stats = Stats {
        views: count(&VIEW_COUNT_PATTERN),
        likes: count(&REACTION_COUNT_PATTERN).or_else(|| count(&REEL_REACTION_COUNT_PATTERN)),
        comments: count(&COMMENT_COUNT_PATTERN).or_else(|| count(&REEL_COMMENT_COUNT_PATTERN)),
        shares: count(&SHARE_COUNT_PATTERN).or_else(|| count(&REEL_SHARE_COUNT_PATTERN)),
    };
    let author = OWNER_ID_PATTERN
        .captures(html)
        .or_else(|| REEL_OWNER_ID_PATTERN.captures(html))
        .map(|captures| author(html, &captures[1]));

    if author.is_none() && stats.likes.is_none() {
        return Err(bad_gateway(
            "Facebook page did not include the post details",
        ));
    }
    Ok((author, stats))
}

fn author(html: &str, id: &str) -> Author {
    let mut profile = Profile::default();
    let blobs: Vec<Value> = relay_blobs(html)
        .filter(|blob| blob.contains(id))
        .filter_map(|blob| serde_json::from_str(blob).ok())
        .collect();
    for map in blobs
        .iter()
        .flat_map(objects)
        .filter(|map| map.get("id").and_then(Value::as_str) == Some(id))
    {
        let text = |key: &str| map.get(key).and_then(Value::as_str).map(str::to_string);
        profile.name = profile.name.take().or_else(|| text("name"));
        profile.url = profile
            .url
            .take()
            .or_else(|| text("url"))
            .or_else(|| text("profile_url"));
        profile.avatar_url = profile.avatar_url.take().or_else(|| {
            map.get("profile_picture")?["uri"]
                .as_str()
                .map(str::to_string)
        });
        profile.verified = profile
            .verified
            .or_else(|| map.get("is_verified").and_then(Value::as_bool));
    }

    let username = profile
        .url
        .as_deref()
        .and_then(|url| Url::parse(url).ok())
        .and_then(|url| Some(url.path_segments()?.next()?.to_string()))
        .filter(|segment| {
            is_name(segment) && !["profile.php", "people"].contains(&segment.as_str())
        })
        .unwrap_or_default();

    Author {
        id: Some(id.to_string()),
        username,
        display_name: profile.name,
        profile_url: profile
            .url
            .or_else(|| Some(format!("https://www.facebook.com/{id}"))),
        avatar_url: profile.avatar_url.as_deref().and_then(cdn_url),
        verified: profile.verified,
    }
}

#[derive(Default)]
struct Profile {
    name: Option<String>,
    url: Option<String>,
    avatar_url: Option<String>,
    verified: Option<bool>,
}

fn find_video<'a>(value: &'a Value, video_id: &str) -> Option<&'a Map<String, Value>> {
    objects(value).find(|map| {
        let id = map
            .get("videoId")
            .or_else(|| map.get("id"))
            .and_then(Value::as_str);
        id == Some(video_id) && FORMAT_KEYS.iter().any(|key| map.contains_key(*key))
    })
}

fn manifests(video: &Value) -> Vec<&str> {
    let legacy = video
        .get("videoDeliveryLegacyFields")
        .filter(|fields| fields.is_object())
        .unwrap_or(video);
    let inline = [
        "dash_manifest",
        "playlist",
        "dash_manifest_xml_string",
        "manifest_xml",
    ]
    .iter()
    .find_map(|key| legacy[*key].as_str());
    let delivered = video["videoDeliveryResponseFragment"]["videoDeliveryResponseResult"]
        ["dash_manifests"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|manifest| manifest["manifest_xml"].as_str());

    inline.into_iter().chain(delivered).collect()
}

struct Representation {
    url: String,
    kind: MediaKind,
    codecs: String,
    width: u32,
    height: u32,
    bandwidth: u32,
}

fn parse_manifest(manifest: &str) -> Vec<Representation> {
    let Ok(document) = roxmltree::Document::parse(manifest) else {
        return Vec::new();
    };
    document
        .descendants()
        .filter(|node| node.has_tag_name("Representation"))
        .filter_map(|node| {
            let parent = node.parent()?;
            let attribute = |name: &str| node.attribute(name).or_else(|| parent.attribute(name));
            let child = |name: &str| node.children().find(|child| child.has_tag_name(name));
            let number = |name: &str| attribute(name).and_then(|value| value.parse().ok());

            let mime = attribute("mimeType").or_else(|| attribute("contentType"))?;
            let kind = if mime.starts_with("video") {
                MediaKind::Video
            } else if mime.starts_with("audio") {
                MediaKind::Audio
            } else {
                return None;
            };
            child("SegmentBase")?;

            Some(Representation {
                url: cdn_url(child("BaseURL")?.text()?.trim())?,
                kind,
                codecs: attribute("codecs").unwrap_or_default().to_string(),
                width: number("width").unwrap_or(0),
                height: number("height").unwrap_or(0),
                bandwidth: number("bandwidth").unwrap_or(0),
            })
        })
        .collect()
}

fn best_video(representations: &[Representation]) -> Option<&Representation> {
    let videos: Vec<&Representation> = representations
        .iter()
        .filter(|representation| representation.kind == MediaKind::Video)
        .collect();
    let has_non_av1 = videos
        .iter()
        .any(|representation| !representation.codecs.starts_with("av01"));

    videos
        .into_iter()
        .filter(|representation| !has_non_av1 || !representation.codecs.starts_with("av01"))
        .max_by_key(|representation| {
            (
                representation.width.min(representation.height),
                representation.bandwidth,
            )
        })
}

fn best_audio(representations: &[Representation]) -> Option<&Representation> {
    representations
        .iter()
        .filter(|representation| representation.kind == MediaKind::Audio)
        .max_by_key(|representation| representation.bandwidth)
}

fn not_cdn() -> HttpError {
    bad_gateway("Extracted media URL was not a Facebook CDN URL")
}

fn meta_tags(html: &str) -> BTreeMap<String, String> {
    let selector = Selector::parse("meta").unwrap();
    let mut tags = BTreeMap::new();

    for element in Html::parse_document(html).select(&selector) {
        let element = element.value();
        let key = element.attr("property").or_else(|| element.attr("name"));
        let content = element
            .attr("content")
            .filter(|content| !content.is_empty());
        if let (Some(key), Some(content)) = (key, content) {
            tags.entry(key.to_lowercase())
                .or_insert_with(|| content.to_string());
        }
    }

    tags
}

fn image(url: String) -> Media {
    Media {
        kind: MediaKind::Image,
        url,
        width: None,
        height: None,
    }
}

fn cdn_url(url: &str) -> Option<String> {
    let mut url = Url::parse(&crate::sources::cdn_url(url, &["fbcdn.net"])?).ok()?;
    if url.query_pairs().any(|(key, _)| key == "ctp") {
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(key, _)| key != "ctp")
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        url.query_pairs_mut().clear().extend_pairs(pairs);
    }
    Some(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use axum::http::StatusCode;
    use serde_json::json;

    #[test]
    fn extracts_single_photo_posts() {
        let node = json!({ "node_v2": { "attachments": [
            { "media": { "__typename": "Photo", "id": "1" }, "styles": { "attachment": { "media": {
                "__typename": "Photo",
                "id": "1",
                "viewer_image": { "width": 640, "height": 627 },
                "photo_image": { "uri": IMAGE_ONE },
            } } } },
        ] } });
        let html = format!("<html><body>{}</body></html>", relay(node));

        let embed = post().embed_from_html(&html).unwrap();
        let urls: Vec<&str> = embed.media.iter().map(|media| media.url.as_str()).collect();

        assert_eq!(urls, [IMAGE_ONE]);
    }

    #[test]
    fn takes_the_video_id_from_video_routes_only() {
        let video = relay(json!({ "video": {
            "id": VIDEO_ID,
            "videoDeliveryLegacyFields": { "browser_native_hd_url": VIDEO_URL },
        } }));
        let params = format!(r#"<script>{{"video_id":"{VIDEO_ID}"}}</script>"#);
        let photos =
            relay(json!({ "node_v2": { "attachments": [{ "media": photo("1", IMAGE_ONE) }] } }));
        let photo_page = format!("<html><body>{params}{photos}{video}</body></html>");
        let video_page = format!(
            r#"<html><body><script>{{{VIDEO_ROUTE_MARKER}}}</script>{params}{video}</body></html>"#
        );

        let photo_post = post().embed_from_html(&photo_page).unwrap();
        let video_post = reel().embed_from_html(&video_page).unwrap();

        assert_eq!(photo_post.media[0].url, IMAGE_ONE);
        assert_eq!(video_post.media[0].url, VIDEO_URL);
    }

    #[test]
    fn extracts_shared_post_photos() {
        let node = json!({ "node_v2": {
            "attachments": [],
            "comet_sections": {
                "content": { "story": { "attached_story": { "attachments": [{ "media": photo("1", IMAGE_ONE) }] } } },
                "feedback": { "comments": [{ "attachments": [{ "media": photo("2", IMAGE_TWO) }] }] },
            },
        } });
        let html = format!("<html><body>{}</body></html>", relay(node));

        let embed = post().embed_from_html(&html).unwrap();
        let urls: Vec<&str> = embed.media.iter().map(|media| media.url.as_str()).collect();

        assert_eq!(urls, [IMAGE_ONE]);
    }

    #[test]
    fn extracts_every_post_photo() {
        for image_count in [2, 3] {
            let embed = post()
                .embed_from_html(&image_post_html(image_count))
                .unwrap();
            let urls: Vec<&str> = embed.media.iter().map(|media| media.url.as_str()).collect();

            assert_eq!(urls, [IMAGE_ONE, IMAGE_TWO, IMAGE_THREE][..image_count]);
            assert!(embed
                .media
                .iter()
                .all(|media| media.kind == MediaKind::Image));
            assert_eq!(embed.description.as_deref(), Some("Facebook post"));
        }
    }

    #[test]
    fn extracts_group_post_video_over_comment_photos() {
        let video = json!({ "video": {
            "id": VIDEO_ID,
            "width": 1080,
            "height": 1920,
            "videoDeliveryLegacyFields": { "browser_native_hd_url": VIDEO_URL },
        } });
        let post = json!({ "node_v2": {
            "attachments": [{ "media": { "__typename": "Video", "id": VIDEO_ID } }],
            "comet_sections": { "comments": [{ "attachments": [{ "media": photo("1", IMAGE_ONE) }] }] },
        } });
        let html = format!(
            r#"<!doctype html><html><head>
            <meta property="og:url" content="https://www.facebook.com/groups/1067513254080794/posts/2378639352968171/">
            </head><body>{}{}</body></html>"#,
            relay(post),
            relay(video)
        );

        let embed = source(&format!("https://www.facebook.com/share/v/{SHARE_CODE}/"))
            .embed_from_html(&html)
            .unwrap();

        assert_eq!(embed.media.len(), 1);
        assert_eq!(embed.media[0].url, VIDEO_URL);
    }

    #[test]
    fn extracts_reel_video() {
        let embed = reel().embed_from_html(&reel_html()).unwrap();
        let video = &embed.media[0];

        assert_eq!(embed.media.len(), 1);
        assert_eq!(video.kind, MediaKind::Video);
        assert_eq!(video.url, VIDEO_URL);
        assert_eq!((video.width, video.height), (Some(1080), Some(1920)));
    }

    #[test]
    fn extracts_photo_page_without_og_tags() {
        let photo_page = relay(json!({ "currMedia": {
            "__typename": "Photo",
            "id": "1",
            "image": { "uri": IMAGE_ONE },
            "creation_story": { "message": { "text": "Photo caption" } },
        } }));
        let embed = source(&format!("https://www.facebook.com/share/{SHARE_CODE}/"))
            .embed_from_html(&photo_page)
            .unwrap();

        assert_eq!(embed.media.len(), 1);
        assert_eq!(embed.media[0].url, IMAGE_ONE);
        assert_eq!(embed.media[0].kind, MediaKind::Image);
        assert_eq!(embed.description.as_deref(), Some("Photo caption"));
    }

    #[test]
    fn rejects_page_without_media() {
        let HttpError(status, _) = reel()
            .embed_from_html("<!doctype html><html></html>")
            .err()
            .unwrap();

        assert_eq!(status, StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn reads_the_story_card_from_its_bucket() {
        let source = source(
            "https://www.facebook.com/stories/140936254287261/UzpfSVNDOjE3NTgyMjIyOTg4MTM5MTE=/",
        );
        let card = |token: &str, photo: &str| {
            json!({ "node": {
                "id": token,
                "attachments": [{ "media": { "__typename": "Photo", "id": photo, "image": { "uri": photo } } }],
                "actors": [{ "__typename": "User", "id": "100051126609171" }],
                "message": { "text": "hello" },
                "story_card_info": { "feedback_summary": { "total_reaction_count": 7 } },
            } })
        };
        let html = relay(json!({
            "story_bucket_owner": {
                "id": "100051126609171",
                "name": "Ali Shtayeh",
                "url": "https://www.facebook.com/ali.hshtayeh",
                "profile_picture": { "uri": IMAGE_THREE },
            },
            "unified_stories_with_notes": { "edges": [
                card("UzpfSVNDOjE3NzEzMjczMTc0NzIzNjU=", IMAGE_TWO),
                card("UzpfSVNDOjE3NTgyMjIyOTg4MTM5MTE=", IMAGE_ONE),
            ] },
        }));

        let post = source.embed_from_html(&html).unwrap();
        assert_eq!(
            post.media
                .iter()
                .map(|media| media.url.as_str())
                .collect::<Vec<_>>(),
            [IMAGE_ONE]
        );
        let author = post.author.unwrap();
        assert_eq!(author.display_name.as_deref(), Some("Ali Shtayeh"));
        assert_eq!(author.username, "ali.hshtayeh");
        assert_eq!(author.avatar_url.as_deref(), Some(IMAGE_THREE));
        assert_eq!(post.description.as_deref(), Some("hello"));
        assert_eq!(post.stats.and_then(|stats| stats.likes), Some(7));
    }

    #[test]
    fn drops_the_display_size_from_photo_urls() {
        assert_eq!(
            cdn_url("https://scontent-lhr6-1.xx.fbcdn.net/v/t39.99422-6/a_n.png?stp=dst-jpg_tt6&cstp=mx1600x2000&ctp=s640x640&oh=00_AQ&oe=6AE7").as_deref(),
            Some("https://scontent-lhr6-1.xx.fbcdn.net/v/t39.99422-6/a_n.png?stp=dst-jpg_tt6&cstp=mx1600x2000&oh=00_AQ&oe=6AE7")
        );
        assert_eq!(cdn_url(VIDEO_URL).as_deref(), Some(VIDEO_URL));
    }

    #[test]
    fn only_accepts_facebook_cdn_urls() {
        assert!(cdn_url(VIDEO_URL).is_some());
        assert!(cdn_url("https://fbcdn.net.evil.example/video.mp4").is_none());
        assert!(cdn_url("http://video.xx.fbcdn.net/video.mp4").is_none());

        let html = reel_html().replace("video-lhr8-1.xx.fbcdn.net", "evil.example.com");
        assert!(reel().embed_from_html(&html).is_err());
    }

    #[test]
    fn extracts_post_author_and_counts() {
        let html = r#"<script data-sjs>{"feedback":{"reaction_count":{"count":1028},"share_count":{"count":92},"comment_rendering_instance":{"comments":{"total_count":14}},"owning_profile":{"__typename":"User","name":"LADbible","id":"100064556895235"}}}</script>
            <script data-sjs>{"related":{"reaction_count":{"count":5}},"actors":[{"profile_picture":{"uri":"https:\/\/scontent.xx.fbcdn.net\/avatar.jpg"},"id":"100064556895235","name":"LADbible","url":"https:\/\/www.facebook.com\/LADbible","is_verified":true}]}</script>"#;
        let (author, stats) = post_details(html).unwrap();
        let author = author.unwrap();

        assert_eq!(author.username, "LADbible");
        assert_eq!(author.display_name.as_deref(), Some("LADbible"));
        assert_eq!(
            author.avatar_url.as_deref(),
            Some("https://scontent.xx.fbcdn.net/avatar.jpg")
        );
        assert_eq!(author.verified, Some(true));
        assert_eq!(
            (stats.likes, stats.shares, stats.comments),
            (Some(1028), Some(92), Some(14))
        );
    }

    #[test]
    fn extracts_reel_author_and_counts() {
        let html = r#"<script data-sjs>{"video":{"owner":{"__typename":"User","id":"615944"}},"feedback":{"total_comment_count":6,"share_count_reduced":"97"},"fb_reel_react_button":{"story":{"feedback":{"unified_reactors":{"count":1099}}}}}</script>
            <script data-sjs>{"actors":[{"id":"615944","name":"nikishei"}]}</script>"#;
        let (author, stats) = post_details(html).unwrap();

        assert_eq!(author.unwrap().display_name.as_deref(), Some("nikishei"));
        assert_eq!(
            (stats.likes, stats.shares, stats.comments),
            (Some(1099), Some(97), Some(6))
        );
    }

    #[test]
    fn ignores_people_urls_as_usernames() {
        let html = r#"<script data-sjs>{"owning_profile":{"__typename":"User","name":"Lovememehub","id":"61575146079335"},"reaction_count":{"count":1}}</script>
            <script data-sjs>{"actors":[{"id":"61575146079335","name":"Lovememehub","url":"https:\/\/www.facebook.com\/people\/Lovememehub\/61575146079335\/"}]}</script>"#;
        let author = post_details(html).unwrap().0.unwrap();

        assert_eq!(author.username, "");
        assert_eq!(crate::sources::author_name(&author), "Lovememehub");
    }

    #[test]
    fn names_authors_without_a_vanity_url() {
        let html = r#"<script data-sjs>{"owning_profile":{"__typename":"User","name":"Krista","short_name":"K","id":"pfbid0Ws"},"reaction_count":{"count":1}}</script>"#;
        let author = post_details(html).unwrap().0.unwrap();

        assert_eq!(crate::sources::author_name(&author), "Krista");
        assert_eq!(
            author.profile_url.as_deref(),
            Some("https://www.facebook.com/pfbid0Ws")
        );
    }

    const DASH_AV1: &str = "https://video.xx.fbcdn.net/av1.mp4?a=1&b=2";
    const DASH_AVC: &str = "https://video.xx.fbcdn.net/avc.mp4";
    const DASH_AVC_SMALL: &str = "https://video.xx.fbcdn.net/avc-small.mp4";
    const DASH_AUDIO: &str = "https://video.xx.fbcdn.net/audio.mp4";
    const DASH_AUDIO_LOW: &str = "https://video.xx.fbcdn.net/audio-low.mp4";

    fn representation(codecs: &str, size: (u32, u32), bandwidth: u32, url: &str) -> String {
        format!(
            r#"<Representation codecs="{codecs}" width="{}" height="{}" bandwidth="{bandwidth}"><BaseURL>{}</BaseURL><SegmentBase indexRange="826-905"><Initialization range="0-825"/></SegmentBase></Representation>"#,
            size.0,
            size.1,
            url.replace('&', "&amp;")
        )
    }

    fn mpd(videos: &[String], audios: &[String]) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><MPD xmlns="urn:mpeg:dash:schema:mpd:2011"><Period><AdaptationSet contentType="video" mimeType="video/mp4">{}</AdaptationSet><AdaptationSet contentType="audio" mimeType="audio/mp4">{}</AdaptationSet></Period></MPD>"#,
            videos.concat(),
            audios.concat()
        )
    }

    // the video node sits deep inside a relay payload, next to unrelated videos
    fn watch_page(video_id: &str, manifest: &str) -> String {
        let payload = json!({ "require": [["ScheduledServerJS", { "__bbox": { "result": { "data": [
            { "id": "999", "videoDeliveryLegacyFields": { "dash_manifest_xml_string": mpd(&[], &[]) } },
            { "id": video_id, "videoDeliveryLegacyFields": {
                "dash_manifest_xml_string": manifest,
                "browser_native_sd_url": VIDEO_URL,
            } },
        ] } } }]] });
        format!(
            r#"<html><body><script type="application/json" data-sjs>{payload}</script></body></html>"#
        )
    }

    fn query(url: &str) -> BTreeMap<String, String> {
        Url::parse(url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect()
    }

    #[test]
    fn merges_best_dash_video_and_audio() {
        let manifest = mpd(
            &[
                representation("av01.0.08M.08", (1080, 1920), 3_000_000, DASH_AV1),
                representation("avc1.64001F", (720, 1280), 1_500_000, DASH_AVC),
                representation("avc1.64001F", (540, 960), 2_000_000, DASH_AVC_SMALL),
                representation(
                    "avc1.640028",
                    (1080, 1920),
                    9_000_000,
                    "https://evil.example.com/video.mp4",
                ),
            ],
            &[
                representation("mp4a.40.5", (0, 0), 62_000, DASH_AUDIO_LOW),
                representation("mp4a.40.2", (0, 0), 128_000, DASH_AUDIO),
            ],
        );
        let video = video(&watch_page(VIDEO_ID, &manifest), VIDEO_ID).unwrap();
        let query = query(&video.url);

        assert!(video.url.starts_with("https://proxy.abembed.com/merge?"));
        assert_eq!(query["video"], DASH_AVC);
        assert_eq!(query["audio"], DASH_AUDIO);
        assert_eq!((video.width, video.height), (Some(720), Some(1280)));
    }

    #[test]
    fn uses_av1_when_it_is_the_only_video() {
        let manifest = mpd(
            &[representation(
                "av01.0.08M.08",
                (1080, 1920),
                3_000_000,
                DASH_AV1,
            )],
            &[representation("mp4a.40.5", (0, 0), 62_000, DASH_AUDIO)],
        );
        let video = video(&watch_page(VIDEO_ID, &manifest), VIDEO_ID).unwrap();

        assert_eq!(query(&video.url)["video"], DASH_AV1);
    }

    #[test]
    fn falls_back_to_progressive_video_without_a_dash_pair() {
        let video_only = mpd(
            &[representation(
                "avc1.64001F",
                (720, 1280),
                1_500_000,
                DASH_AVC,
            )],
            &[],
        );

        for manifest in [video_only.as_str(), "not xml"] {
            let video = video(&watch_page(VIDEO_ID, manifest), VIDEO_ID).unwrap();
            assert_eq!(video.url, VIDEO_URL);
        }
    }

    #[test]
    fn retries_pages_without_the_video() {
        let manifest = mpd(
            &[representation(
                "avc1.64001F",
                (720, 1280),
                1_500_000,
                DASH_AVC,
            )],
            &[representation("mp4a.40.2", (0, 0), 128_000, DASH_AUDIO)],
        );
        let HttpError(status, _) = video(&watch_page("123", &manifest), VIDEO_ID)
            .err()
            .unwrap();

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(video("<!doctype html><html></html>", VIDEO_ID).is_err());
    }

    #[test]
    fn finds_video_ids_in_post_links() {
        for (url, id) in [
            (
                "https://www.facebook.com/reel/1551678869998267/",
                Some(VIDEO_ID),
            ),
            (
                "https://www.facebook.com/watch/?v=1551678869998267",
                Some(VIDEO_ID),
            ),
            (
                "https://www.facebook.com/kuli.y.mas/videos/some-title/1551678869998267/",
                Some(VIDEO_ID),
            ),
            (
                "https://www.facebook.com/kuli.y.mas/posts/1405946908140934/",
                None,
            ),
        ] {
            let found = VIDEO_ID_PATTERN
                .captures(url)
                .map(|captures| captures[1].to_string());
            assert_eq!(found.as_deref(), id, "{url}");
        }
    }
}
