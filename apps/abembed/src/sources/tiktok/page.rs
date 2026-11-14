// parsing of the tiktok pages the provider workers fetch

use super::{TikTokSource, APP_PLAY_HOST};
use crate::sources::{bad_gateway, Author, EmbedPost, HttpError, Media, MediaKind, Source, Stats};
use scraper::{Html, Selector};
use serde_json::Value;
use worker::Url;

// fixes the mp4 timing tiktok cdn files get wrong, see apps/proxy
const VIDEO_PROXY_URL: &str = "https://proxy.abembed.com/video";

impl TikTokSource {
    // the video with, for dash streams, the separate audio the proxy merges in
    pub(super) fn video_from_page(
        &self,
        html: &str,
    ) -> Result<(EmbedPost, Option<String>), HttpError> {
        let item = script_json(html, "__UNIVERSAL_DATA_FOR_REHYDRATION__")
            .and_then(|json| {
                json.pointer("/__DEFAULT_SCOPE__/webapp.video-detail/itemInfo/itemStruct")
                    .cloned()
            })
            .ok_or_else(|| bad_gateway("TikTok page did not include video data"))?;
        if item["id"].as_str() != Some(self.id.as_str()) {
            return Err(bad_gateway("TikTok page returned a different video"));
        }

        let video = &item["video"];
        let mut bitrates: Vec<&Value> = video["bitrateInfo"]
            .as_array()
            .into_iter()
            .flatten()
            .collect();
        bitrates.sort_by_key(|bitrate| std::cmp::Reverse(quality(bitrate)));
        // dash entries are video only, their audio is a separate stream
        let dash_audio = video["bitrateAudioInfo"]
            .as_array()
            .into_iter()
            .flatten()
            .max_by_key(|audio| count(&audio["Bitrate"]))
            .and_then(|audio| app_play_url(audio["UrlList"]["FallbackUrl"].as_str()?));
        let (url, audio) = bitrates
            .iter()
            .filter(|bitrate| bitrate["Format"] != "dash" || dash_audio.is_some())
            .find_map(|bitrate| {
                let url = urls(&bitrate["PlayAddr"]["UrlList"])
                    .into_iter()
                    .find_map(app_play_url)?;
                let audio = dash_audio.clone().filter(|_| bitrate["Format"] == "dash");
                Some((url, audio))
            })
            .or_else(|| {
                let url = urls(&video["playAddr"])
                    .into_iter()
                    .chain(urls(&video["downloadAddr"]))
                    .chain(
                        bitrates
                            .iter()
                            .flat_map(|bitrate| urls(&bitrate["PlayAddr"]["UrlList"])),
                    )
                    .find_map(cdn_url)?;
                Some((url, None))
            })
            .ok_or_else(|| bad_gateway("TikTok page did not include a direct video URL"))?;
        let stats = if item["stats"].is_object() {
            &item["stats"]
        } else {
            &item["statsV2"]
        };

        let mut embed = self.embed(
            &item["desc"],
            &item["author"],
            stats,
            vec![new_media(MediaKind::Video, url, video)],
        );
        embed.created_at = count(&item["createTime"]).or(embed.created_at);

        Ok((embed, audio))
    }

    pub(super) fn video_from_embed(&self, html: &str) -> Result<EmbedPost, HttpError> {
        let data = self
            .embed_video_data(html)
            .ok_or_else(|| bad_gateway("TikTok embed did not include video data"))?;
        let info = &data["itemInfos"];
        if info["id"].as_str() != Some(self.id.as_str()) {
            return Err(bad_gateway("TikTok embed returned a different video"));
        }

        let url = urls(&info["video"]["urls"])
            .into_iter()
            .find_map(cdn_url)
            .ok_or_else(|| bad_gateway("TikTok embed did not include a video URL"))?;

        Ok(self.embed(
            &info["text"],
            &data["authorInfos"],
            info,
            vec![new_media(
                MediaKind::Video,
                proxied_video_url(&url),
                &info["video"]["videoMeta"],
            )],
        ))
    }

    pub(super) fn photo_from_embed(&self, html: &str) -> Result<EmbedPost, HttpError> {
        let data = self
            .embed_video_data(html)
            .ok_or_else(|| bad_gateway("TikTok embed did not include photo data"))?;
        let info = &data["itemInfos"];

        let mut images: Vec<Media> = data["imagePostInfo"]["displayImages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|image| {
                let url = urls(&image["urlList"]).into_iter().find_map(cdn_url)?;
                Some(new_media(MediaKind::Image, url, image))
            })
            .collect();
        if images.is_empty() {
            images.extend(
                urls(&info["coversOrigin"])
                    .into_iter()
                    .chain(urls(&info["covers"]))
                    .find_map(cdn_url)
                    .map(|url| new_media(MediaKind::Image, url, &Value::Null)),
            );
        }
        if images.is_empty() {
            return Err(bad_gateway("TikTok embed did not include a photo URL"));
        }

        Ok(self.embed(&info["text"], &data["authorInfos"], info, images))
    }

    fn embed_video_data(&self, html: &str) -> Option<Value> {
        let json = script_json(html, "__FRONTITY_CONNECT_STATE__")?;
        let data = &json["source"]["data"][format!("/embed/v2/{}", self.id)]["videoData"];
        data.is_object().then(|| data.clone())
    }

    fn embed(
        &self,
        description: &Value,
        author: &Value,
        stats: &Value,
        media: Vec<Media>,
    ) -> EmbedPost {
        EmbedPost {
            provider: "tiktok".to_string(),
            provider_id: self.id.clone(),
            source_url: self.source_url(),
            title: None,
            description: non_empty_str(description).map(str::to_string),
            author: Some(self.author(author)),
            media,
            stats: Some(Stats {
                views: count(&stats["playCount"]),
                likes: count(&stats["diggCount"]),
                comments: count(&stats["commentCount"]),
                shares: count(&stats["shareCount"]),
            }),
            lang: None,
            reply_to: None,
            quote: None,
            translation: None,
            // embed pages have no date, ids hold the second their upload began above 32 bits
            created_at: self.id.parse::<u64>().ok().map(|id| id >> 32),
        }
    }

    fn author(&self, author: &Value) -> Author {
        let username = non_empty_str(&author["uniqueId"]).unwrap_or(&self.username);
        let display_name = ["nickname", "nickName"]
            .iter()
            .find_map(|key| non_empty_str(&author[*key]));
        let avatar_url = ["avatarThumb", "avatarMedium", "covers"]
            .iter()
            .flat_map(|key| urls(&author[*key]))
            .find_map(cdn_url);

        Author {
            id: None,
            username: username.to_string(),
            display_name: display_name.map(str::to_string),
            profile_url: Some(format!("https://www.tiktok.com/@{username}")),
            avatar_url,
            verified: author["verified"].as_bool(),
        }
    }
}

fn script_json(html: &str, id: &str) -> Option<Value> {
    let selector = Selector::parse(&format!("script#{id}")).unwrap();
    Html::parse_document(html)
        .select(&selector)
        .find_map(|script| serde_json::from_str(&script.text().collect::<String>()).ok())
}

// a string, an array, or `{ urlList: [...] }`
fn urls(value: &Value) -> Vec<&str> {
    match value {
        Value::String(url) => vec![url],
        Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
        Value::Object(record) => record.get("urlList").map(urls).unwrap_or_default(),
        _ => Vec::new(),
    }
}

// highest resolution first, then highest bitrate
fn quality(bitrate: &Value) -> (u64, u64) {
    let addr = &bitrate["PlayAddr"];
    let pixels = count(&addr["Width"]).unwrap_or(0) * count(&addr["Height"]).unwrap_or(0);
    (pixels, count(&bitrate["Bitrate"]).unwrap_or(0))
}

fn new_media(kind: MediaKind, url: String, meta: &Value) -> Media {
    let dimension = |key: &str| count(&meta[key]).and_then(|value| u32::try_from(value).ok());

    Media {
        kind,
        url,
        width: dimension("width"),
        height: dimension("height"),
        alt: None,
    }
}

// numbers in some payloads, numeric strings in others
fn count(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}

fn non_empty_str(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| !value.is_empty())
}

pub(super) fn cdn_url(url: &str) -> Option<String> {
    crate::sources::cdn_url(
        url,
        &["tiktokcdn.com", "tiktokcdn-us.com", "tiktokcdn-eu.com"],
    )
}

fn app_play_url(url: &str) -> Option<String> {
    let mut url = Url::parse(url).ok()?;
    if url.scheme() != "https"
        || url.path() != "/aweme/v1/play/"
        || !matches!(
            url.host_str(),
            Some("www.tiktok.com" | "api.tiktokv.com" | APP_PLAY_HOST)
        )
    {
        return None;
    }

    let query: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(key, _)| key != "faid" && key != "source")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    url.set_host(Some(APP_PLAY_HOST)).ok()?;
    url.query_pairs_mut()
        .clear()
        .extend_pairs(query)
        .append_pair("faid", "1233")
        .append_pair("source", "FEED");

    Some(url.to_string())
}

pub(super) fn proxied_video_url(url: &str) -> String {
    Url::parse_with_params(VIDEO_PROXY_URL, [("url", url)])
        .unwrap()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use axum::http::StatusCode;
    use serde_json::json;
    use std::collections::BTreeMap;

    #[test]
    fn extracts_video_page_with_app_play_url() {
        let (embed, _) = source("video", VIDEO_ID)
            .video_from_page(&video_page())
            .unwrap();
        let video = &embed.media[0];
        let play_url = Url::parse(&video.url).unwrap();
        let query: BTreeMap<_, _> = play_url.query_pairs().into_owned().collect();
        let author = embed.author.as_ref().unwrap();
        let stats = embed.stats.as_ref().unwrap();

        assert_eq!(play_url.host_str(), Some(APP_PLAY_HOST));
        assert_eq!(play_url.path(), "/aweme/v1/play/");
        assert_eq!(query["faid"], "1233");
        assert_eq!(query["source"], "FEED");
        assert_eq!(query["item_id"], VIDEO_ID);
        // the 1080p dash entry beats the 720p mp4 listed before it
        assert_eq!(query["file_id"], "d2f2");
        assert_eq!((video.width, video.height), (Some(720), Some(1280)));
        assert_eq!(embed.description.as_deref(), Some("Coffee time"));
        assert_eq!(embed.created_at, Some(1_784_688_572));
        assert_eq!(author.display_name.as_deref(), Some("Kopi Lawak"));
        assert_eq!(author.avatar_url.as_deref(), Some(AVATAR_URL));
        assert_eq!(
            (stats.likes, stats.shares, stats.comments, stats.views),
            (Some(21), Some(5), Some(8), Some(100))
        );
    }

    #[test]
    fn pairs_dash_video_with_its_best_audio_stream() {
        let (_, audio) = source("video", VIDEO_ID)
            .video_from_page(&video_page())
            .unwrap();
        let audio = Url::parse(&audio.unwrap()).unwrap();
        let query: BTreeMap<_, _> = audio.query_pairs().into_owned().collect();

        assert_eq!(audio.host_str(), Some(APP_PLAY_HOST));
        assert_eq!(query["file_id"], "2119");
        assert_eq!(query["faid"], "1233");
    }

    #[test]
    fn skips_dash_video_without_an_audio_stream() {
        let (embed, audio) = source("video", VIDEO_ID)
            .video_from_page(&video_page_without_audio())
            .unwrap();
        let play_url = Url::parse(&embed.media[0].url).unwrap();
        let query: BTreeMap<_, _> = play_url.query_pairs().into_owned().collect();

        assert_eq!(query["file_id"], "37a4");
        assert_eq!(audio, None);
    }

    #[test]
    fn rejects_video_page_for_a_different_video() {
        let HttpError(status, _) = source("video", "7665179028352945427")
            .video_from_page(&video_page())
            .unwrap_err();

        assert_eq!(status, StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn rejects_video_page_without_hydration() {
        assert!(source("video", VIDEO_ID)
            .video_from_page("<!doctype html><html></html>")
            .is_err());
    }

    #[test]
    fn extracts_video_from_embed_page() {
        let html = embed_page(
            VIDEO_ID,
            json!({
                "authorInfos": { "uniqueId": USERNAME, "nickName": "Kopi Lawak", "covers": [AVATAR_URL] },
                "itemInfos": {
                    "id": VIDEO_ID,
                    "text": "Coffee time",
                    "diggCount": 21,
                    "video": { "urls": [EMBED_VIDEO_URL], "videoMeta": { "width": 720, "height": 1280 } },
                },
            }),
        );
        let embed = source("video", VIDEO_ID).video_from_embed(&html).unwrap();

        assert_eq!(embed.media[0].url, proxied_video_url(EMBED_VIDEO_URL));
        assert_eq!(embed.media[0].width, Some(720));
        assert_eq!(embed.created_at, Some(1_784_688_566));
        assert_eq!(embed.stats.unwrap().likes, Some(21));
    }

    #[test]
    fn extracts_all_photo_images_from_embed_page() {
        let embed = source("photo", PHOTO_ID)
            .photo_from_embed(&photo_embed_page())
            .unwrap();
        let urls: Vec<&str> = embed.media.iter().map(|media| media.url.as_str()).collect();
        let author = embed.author.as_ref().unwrap();
        let stats = embed.stats.as_ref().unwrap();

        assert_eq!(urls, [IMAGE_ONE, IMAGE_TWO]);
        assert_eq!(
            (embed.media[0].width, embed.media[0].height),
            (Some(814), Some(376))
        );
        assert!(embed
            .media
            .iter()
            .all(|media| media.kind == MediaKind::Image));
        assert_eq!(embed.description.as_deref(), Some("Photo description"));
        assert_eq!(author.display_name.as_deref(), Some("Photo Author"));
        assert_eq!(author.avatar_url.as_deref(), Some(AVATAR_URL));
        assert_eq!(
            (stats.likes, stats.shares, stats.comments),
            (Some(31), Some(6), Some(9))
        );
    }

    #[test]
    fn ignores_embed_state_for_other_posts() {
        assert!(source("photo", "7666981398800108814")
            .photo_from_embed(&photo_embed_page())
            .is_err());
    }

    #[test]
    fn only_accepts_tiktok_cdn_urls() {
        assert!(cdn_url(EMBED_VIDEO_URL).is_some());
        assert!(cdn_url(WEBAPP_VIDEO_URL).is_none());
        assert!(cdn_url("https://tiktokcdn.com.evil.example/video.mp4").is_none());
        assert!(cdn_url("http://v16m.tiktokcdn-us.com/video.mp4").is_none());
    }

    #[test]
    fn proxies_video_urls_with_their_query_intact() {
        let proxied = Url::parse(&proxied_video_url(EMBED_VIDEO_URL)).unwrap();
        let (_, url) = proxied.query_pairs().find(|(key, _)| key == "url").unwrap();

        assert_eq!(proxied.host_str(), Some("proxy.abembed.com"));
        assert_eq!(url, EMBED_VIDEO_URL);
    }
}
