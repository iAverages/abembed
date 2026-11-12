// parsing of the embed.reddit.com pages the provider workers fetch

use super::RedditSource;
use crate::sources::{
    bad_gateway, cdn_url, html::INLINE_MEDIA, merged_video_url, sized, Author, EmbedPost,
    HttpError, Media, MediaKind, Source, Stats, GIF_SERVICE_URL,
};
use regex::Regex;
use scraper::{ElementRef, Html, Selector};
use serde_json::Value;
use worker::Url;

const VIDEO_HOST: &str = "https://v.redd.it/";

impl RedditSource {
    // videos are left as their `v.redd.it/{id}` link, see `dash_id`
    pub(super) fn embed_from_html(&self, html: &str) -> Result<EmbedPost, HttpError> {
        let page = Html::parse_document(html);
        let data = select(&page.root_element(), "shreddit-screenview-data")
            .and_then(|element| serde_json::from_str::<Value>(element.attr("data")?).ok())
            .ok_or_else(|| bad_gateway("Reddit embed did not include post data"))?;
        let (thing, id) = match &self.comment {
            Some(comment) => (&data["comment"], format!("t1_{comment}")),
            None => (&data["post"], format!("t3_{}", self.id)),
        };
        if thing["id"].as_str() != Some(id.as_str()) {
            return Err(bad_gateway("Reddit embed returned a different post"));
        }

        let root = page.root_element();
        let (description, mut media) = select(&root, &format!("#{id}-post-rtjson-content"))
            .map(|body| body_content(&body))
            .unwrap_or_default();
        let title = match &self.comment {
            // comment embeds do not include the post title
            Some(_) => None,
            None => {
                if media.is_empty() {
                    media = post_media(&root, &data["post"]);
                }
                select(&root, "h1, shreddit-embed-title")
                    .map(|title| text(&title))
                    .filter(|title| !title.is_empty())
            }
        };

        Ok(EmbedPost {
            provider: "reddit".to_string(),
            provider_id: self.id().to_string(),
            source_url: self.source_url(),
            title,
            description,
            // comment embeds link the author's avatar first
            author: root
                .select(&selector(r#"a[href*="/user/"]"#))
                .map(|link| text(&link))
                .find(|name| !name.is_empty())
                .map(author),
            media,
            stats: Some(Stats {
                likes: select(&root, "faceplate-number[number]")
                    .and_then(|number| number.attr("number")?.parse().ok()),
                comments: comment_count(html),
                ..Stats::default()
            }),
            lang: None,
            reply_to: None,
            quote: None,
            translation: None,
        })
    }
}

pub(super) fn author(username: String) -> Author {
    Author {
        id: None,
        profile_url: Some(format!("https://www.reddit.com/user/{username}/")),
        username,
        display_name: None,
        avatar_url: None,
        verified: None,
    }
}

fn post_media(root: &ElementRef, post: &Value) -> Vec<Media> {
    let gallery: Vec<Media> = root
        .select(&selector("gallery-carousel img"))
        .filter_map(|image| preview_image(image.attr("src")?))
        .collect();
    if !gallery.is_empty() {
        return gallery;
    }

    root.select(&selector("shreddit-player"))
        .find_map(|player| player_media(&player))
        // image posts and "Not supported post" embeds only have the media in the post data
        .or_else(|| linked_media(post["url"].as_str()?))
        .into_iter()
        .collect()
}

fn body_content(body: &ElementRef) -> (Option<String>, Vec<Media>) {
    let mut media = Vec::new();
    let mut blocks = Vec::new();

    for block in body.child_elements() {
        if block.value().name() != "figure" {
            blocks.push(text(&block));
            continue;
        }

        let caption = select(&block, "figcaption")
            .map(|caption| text(&caption))
            .filter(|caption| !caption.is_empty());

        if let Some(figure) = figure_media(&block) {
            media.push(Media {
                alt: caption.clone(),
                ..figure
            });
            blocks.push(INLINE_MEDIA.to_string());
        }

        blocks.extend(caption.map(|caption| format!("-# {caption}")));
    }

    blocks.retain(|block| !block.is_empty());

    while blocks.last().is_some_and(|block| block == INLINE_MEDIA) {
        blocks.pop();
    }

    ((!blocks.is_empty()).then(|| blocks.join("\n\n")), media)
}

fn figure_media(figure: &ElementRef) -> Option<Media> {
    let id = select(figure, r#"[id$="-aspect-ratio"]"#)
        .and_then(|container| container.attr("id")?.strip_suffix("-aspect-ratio"));

    id.and_then(|id| giphy_media(id.strip_prefix("giphy|")?))
        .or_else(|| player_media(&select(figure, "shreddit-player")?))
        .or_else(|| {
            figure
                .select(&selector("img"))
                .find_map(|image| preview_image(image.attr("src")?))
        })
        .or_else(|| linked_media(&format!("{VIDEO_HOST}{}", id?)))
}

fn player_media(player: &ElementRef) -> Option<Media> {
    // gifs play an mp4 preview of the gif
    if player.attr("gif").is_some() {
        let src = player.attr("src")?;
        return preview_image(src).or_else(|| external_gif(src));
    }

    let packaged: Value = serde_json::from_str(player.attr("packaged-media-json")?).ok()?;
    let source = packaged["playbackMp4s"]["permutations"]
        .as_array()?
        .iter()
        .map(|permutation| &permutation["source"])
        .max_by_key(|source| {
            dimension(source, "width").unwrap_or(0) * dimension(source, "height").unwrap_or(0)
        })?;

    Some(Media {
        kind: MediaKind::Video,
        url: cdn_url(source["url"].as_str()?, &["packaged-media.redd.it"])?,
        width: dimension(source, "width"),
        height: dimension(source, "height"),
        alt: None,
    })
}

// `preview.redd.it/{slug}-v0-{file}` and `preview.redd.it/{file}` are resized copies of
// `i.redd.it/{file}`
fn preview_image(src: &str) -> Option<Media> {
    let url = Url::parse(src).ok()?;
    if url.host_str() != Some("preview.redd.it") {
        return None;
    }
    let file = url.path_segments()?.next_back()?.rsplit('-').next()?;
    linked_media(&format!("https://i.redd.it/{file}"))
}

fn external_gif(src: &str) -> Option<Media> {
    let url = Url::parse(&cdn_url(src, &["external-preview.redd.it"])?).ok()?;
    let file = url.path().strip_suffix(".gif")?;

    Some(Media {
        kind: MediaKind::Image,
        url: format!(
            "{GIF_SERVICE_URL}/external-preview{file}.webp?{}",
            url.query()?
        ),
        width: None,
        height: None,
        alt: None,
    })
}

fn giphy_media(id: &str) -> Option<Media> {
    sized(id, 5..=32, |byte| byte.is_ascii_alphanumeric()).then(|| Media {
        kind: MediaKind::Image,
        url: format!("https://i.giphy.com/{id}.gif"),
        width: None,
        height: None,
        alt: None,
    })
}

fn linked_media(url: &str) -> Option<Media> {
    let (kind, url) = match dash_id(url) {
        Some(id) => (MediaKind::Video, format!("{VIDEO_HOST}{id}")),
        None => {
            let url = cdn_url(url, &["i.redd.it"])?;
            let file = url.strip_prefix("https://i.redd.it/")?;
            if !is_media_id(file.split_once('.')?.0) {
                return None;
            }
            (MediaKind::Image, url)
        }
    };

    Some(Media {
        kind,
        url,
        width: None,
        height: None,
        alt: None,
    })
}

// a `v.redd.it/{id}` video still to be resolved through its dash playlist
pub(super) fn dash_id(url: &str) -> Option<&str> {
    url.strip_prefix(VIDEO_HOST)
        .map(|id| id.trim_end_matches('/'))
        .filter(|id| is_media_id(id))
}

// the best video and audio streams, merged by the proxy since dash streams are video only
pub(super) fn dash_video(playlist: &str, id: &str) -> Result<Media, HttpError> {
    let invalid = || bad_gateway("Reddit video playlist was not valid");
    let document = roxmltree::Document::parse(playlist).map_err(|_| invalid())?;
    let base = Url::parse(&format!("{VIDEO_HOST}{id}/")).map_err(|_| invalid())?;
    let representations = |kind: &str| {
        document
            .descendants()
            .filter(|node| node.has_tag_name("Representation"))
            .filter(|node| {
                node.attribute("mimeType")
                    .is_some_and(|mime| mime.starts_with(kind))
            })
            .filter_map(|node| {
                let file = node
                    .children()
                    .find(|child| child.has_tag_name("BaseURL"))?
                    .text()?;
                let number = |name: &str| node.attribute(name)?.parse::<u32>().ok();
                Some((
                    base.join(file.trim()).ok()?.to_string(),
                    number("width"),
                    number("height"),
                    number("bandwidth").unwrap_or(0),
                ))
            })
            .collect::<Vec<_>>()
    };

    let (video, width, height, _) = representations("video/")
        .into_iter()
        .max_by_key(|(_, width, height, bandwidth)| {
            (width.unwrap_or(0) * height.unwrap_or(0), *bandwidth)
        })
        .ok_or_else(|| bad_gateway("Reddit video playlist had no video stream"))?;
    let audio = representations("audio/")
        .into_iter()
        .max_by_key(|(_, _, _, bandwidth)| *bandwidth);

    Ok(Media {
        kind: MediaKind::Video,
        url: match audio {
            Some((audio, ..)) => merged_video_url(&video, &audio),
            None => video,
        },
        width,
        height,
        alt: None,
    })
}

pub(super) fn oembed(json: &str) -> Result<(Option<String>, Option<String>), HttpError> {
    let json: Value =
        serde_json::from_str(json).map_err(|_| bad_gateway("Reddit oembed was not json"))?;
    let field = |key: &str| {
        json[key]
            .as_str()
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    Ok((field("title"), field("author_name")))
}

pub(super) fn feed_body(feed: &str, id: &str) -> Result<Option<String>, HttpError> {
    let document =
        roxmltree::Document::parse(feed).map_err(|_| bad_gateway("Reddit feed was not valid"))?;

    let child = |entry: &roxmltree::Node, name: &str| {
        let child = entry.children().find(|child| child.has_tag_name(name))?;
        child.text().map(str::to_string)
    };

    let content = document
        .descendants()
        .filter(|node| node.has_tag_name("entry"))
        .find(|entry| child(entry, "id") == Some(format!("t3_{id}")))
        .and_then(|entry| child(&entry, "content"));

    Ok(content.and_then(|content| {
        let content = Html::parse_fragment(&content);
        select(&content.root_element(), "div.md").and_then(|body| body_content(&body).0)
    }))
}

fn comment_count(html: &str) -> Option<u64> {
    let count = Regex::new(r"View (\d+) comments?").unwrap();
    match count.captures(html) {
        Some(captures) => captures[1].parse().ok(),
        // shown instead of the count when there are no comments
        None => html.contains("View more on Reddit").then_some(0),
    }
}

fn text(element: &ElementRef) -> String {
    element
        .text()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn dimension(source: &Value, key: &str) -> Option<u32> {
    u32::try_from(source["dimensions"][key].as_u64()?).ok()
}

fn is_media_id(id: &str) -> bool {
    sized(id, 5..=20, |byte| byte.is_ascii_alphanumeric())
}

fn selector(css: &str) -> Selector {
    Selector::parse(css).unwrap()
}

fn select<'a>(element: &ElementRef<'a>, css: &str) -> Option<ElementRef<'a>> {
    element.select(&selector(css)).next()
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use axum::http::StatusCode;

    fn urls(embed: &EmbedPost) -> Vec<&str> {
        embed.media.iter().map(|media| media.url.as_str()).collect()
    }

    #[test]
    fn extracts_image_posts() {
        let embed = source().embed_from_html(&image_post_page()).unwrap();
        let stats = embed.stats.as_ref().unwrap();

        assert_eq!(urls(&embed), ["https://i.redd.it/v1eq2vzfk1zh1.jpeg"]);
        assert_eq!(embed.media[0].kind, MediaKind::Image);
        assert_eq!(
            embed.title.as_deref(),
            Some("I don't like this but it's right")
        );
        assert_eq!(embed.author.unwrap().username, AUTHOR);
        assert_eq!((stats.likes, stats.comments), (Some(9196), Some(1312)));
    }

    #[test]
    fn extracts_full_size_gallery_images() {
        let embed = source().embed_from_html(&gallery_post_page()).unwrap();

        assert_eq!(
            urls(&embed),
            [
                "https://i.redd.it/zx0o44opyzyh1.jpeg",
                "https://i.redd.it/s913bdqpyzyh1.jpeg"
            ]
        );
    }

    #[test]
    fn picks_the_largest_packaged_video() {
        let embed = source().embed_from_html(&video_post_page()).unwrap();
        let video = &embed.media[0];

        assert_eq!(urls(&embed), [PACKAGED_1080]);
        assert_eq!(video.kind, MediaKind::Video);
        assert_eq!((video.width, video.height), (Some(1920), Some(1080)));
    }

    #[test]
    fn serves_external_gifs_through_the_gif_service() {
        let embed = source().embed_from_html(&external_gif_post_page()).unwrap();

        assert_eq!(
            urls(&embed),
            ["https://gif.abembed.com/external-preview/XBb3yF47m8XLmL6NsoHVm0eBNu0uwoXOJ75hevQ7GN8.webp?width=556&format=mp4&s=70a4"]
        );
        assert_eq!(embed.media[0].kind, MediaKind::Image);
    }

    #[test]
    fn extracts_text_posts() {
        let embed = source().embed_from_html(&text_post_page()).unwrap();

        assert!(embed.media.is_empty());
        assert_eq!(
            embed.title.as_deref(),
            Some("Is there a theoretical maximum size in humans (or any animal)?")
        );
        assert_eq!(
            embed.description.as_deref(),
            Some("So people often joke about it but eventually it would cost too much blood right?\n\nIs there some sort of hard limit?")
        );
    }

    #[test]
    fn places_body_images_in_the_post_text() {
        let embed = source().embed_from_html(&inline_media_post_page()).unwrap();

        assert_eq!(
            urls(&embed),
            [
                "https://i.redd.it/uq3ys09fau0i1.png",
                "https://i.redd.it/fl6b1ljpau0i1.png",
                "https://i.redd.it/zx0o44opyzyh1.png"
            ]
        );
        assert_eq!(embed.title.as_deref(), Some("One Year on NixOS"));
        assert_eq!(
            embed.media[0].alt.as_deref(),
            Some("Just me doing some ImHex learning")
        );
        assert_eq!(embed.media[1].alt, None);
        // the image after the last of the text is left to follow it
        assert_eq!(
            embed.description.unwrap(),
            format!(
                "{INLINE_MEDIA}\n\n-# Just me doing some ImHex learning\n\nWell, a little more than one year, actually.\n\n{INLINE_MEDIA}\n\nI'd like to hear your questions.\n\n# Do you use flakes?\n\nYes."
            )
        );
    }

    #[test]
    fn keeps_post_data_media_for_unsupported_posts() {
        let embed = source().embed_from_html(&unsupported_post_page()).unwrap();

        assert_eq!(urls(&embed), ["https://i.redd.it/dm85kmia6myh1.jpeg"]);
        assert_eq!(embed.title, None);
        assert!(embed.author.is_none());
    }

    #[test]
    fn rejects_embeds_for_a_different_post() {
        let other = RedditSource::new(SUBREDDIT, "1xm08ii", None, None).unwrap();
        let HttpError(status, _) = other.embed_from_html(&image_post_page()).unwrap_err();

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(source().embed_from_html("<html></html>").is_err());
    }

    #[test]
    fn extracts_text_comments() {
        let embed = comment_source()
            .embed_from_html(&text_comment_page())
            .unwrap();
        let stats = embed.stats.as_ref().unwrap();

        assert!(embed.media.is_empty());
        assert_eq!(embed.title, None);
        assert_eq!(
            embed.description.as_deref(),
            Some("Furina in r/osugame? damn")
        );
        assert_eq!(embed.author.unwrap().username, "zepol_xela");
        assert_eq!((stats.likes, stats.comments), (Some(1), Some(0)));
    }

    #[test]
    fn extracts_full_size_comment_images() {
        let embed = comment_source()
            .embed_from_html(&image_comment_page())
            .unwrap();

        assert_eq!(urls(&embed), ["https://i.redd.it/p5v2kpn38myh1.png"]);
        assert_eq!(embed.description, None);
    }

    #[test]
    fn extracts_original_comment_gifs() {
        let embed = comment_source()
            .embed_from_html(&gif_comment_page())
            .unwrap();

        assert_eq!(urls(&embed), ["https://i.redd.it/nhvtlii5suxh1.gif"]);
        assert_eq!(embed.stats.unwrap().comments, Some(1));
    }

    #[test]
    fn extracts_giphy_comment_gifs() {
        let embed = comment_source()
            .embed_from_html(&giphy_comment_page())
            .unwrap();

        assert_eq!(urls(&embed), ["https://i.giphy.com/Qx4n8lDS178TEkXlMj.gif"]);
        assert_eq!(embed.media[0].kind, MediaKind::Image);
        assert_eq!(embed.description, None);
    }

    #[test]
    fn leaves_comment_videos_for_their_playlist() {
        let embed = comment_source()
            .embed_from_html(&video_comment_page())
            .unwrap();

        assert_eq!(urls(&embed), ["https://v.redd.it/1wnr3rk6bmyh1"]);
        assert_eq!(dash_id(&embed.media[0].url), Some("1wnr3rk6bmyh1"));
        assert_eq!(
            embed.description.as_deref(),
            Some("The long awaited sequel to")
        );
    }

    #[test]
    fn merges_the_best_dash_video_and_audio() {
        let video = dash_video(DASH_PLAYLIST, "1wnr3rk6bmyh1").unwrap();

        assert_eq!(
            video.url,
            merged_video_url(
                "https://v.redd.it/1wnr3rk6bmyh1/CMAF_720.mp4",
                "https://v.redd.it/1wnr3rk6bmyh1/CMAF_AUDIO_128.mp4"
            )
        );
        assert_eq!((video.width, video.height), (Some(1280), Some(720)));
    }

    #[test]
    fn rejects_playlists_without_video() {
        assert!(dash_video("<MPD></MPD>", "1wnr3rk6bmyh1").is_err());
    }

    #[test]
    fn only_accepts_reddit_media_hosts() {
        assert!(linked_media("https://i.redd.it/v1eq2vzfk1zh1.jpeg").is_some());
        assert!(linked_media("https://i.redd.it.evil.example/v1eq2vzfk1zh1.jpeg").is_none());
        assert!(linked_media("https://www.reddit.com/gallery/1xm08ih").is_none());
        assert!(preview_image("https://evil.example/x-v0-v1eq2vzfk1zh1.jpeg").is_none());
        assert_eq!(dash_id("https://v.redd.it/../evil"), None);
    }

    #[test]
    fn reads_the_post_body_from_the_feed() {
        assert_eq!(
            feed_body(FEED, POST_ID).unwrap().as_deref(),
            Some("For me I find that it's a \"press Q to panic\" button.\n\nHere in this clip you can see why.")
        );
    }

    #[test]
    fn feeds_without_a_post_body_have_none() {
        let no_body = FEED.replace("&lt;div class=&quot;md&quot;&gt;", "&lt;div&gt;");

        assert_eq!(feed_body(&no_body, POST_ID).unwrap(), None);
        assert_eq!(feed_body(FEED, "1xm08ii").unwrap(), None);
        assert!(feed_body("not a feed", POST_ID).is_err());
    }

    #[test]
    fn reads_oembed_title_and_author() {
        assert_eq!(
            oembed(OEMBED).unwrap(),
            (
                Some("Pippi x Furina Yuri".to_string()),
                Some("somecallmehero".to_string())
            )
        );
    }
}
