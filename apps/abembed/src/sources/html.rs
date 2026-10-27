use super::{author_name, Author, EmbedPost, MediaKind, Stats};
use axum::{
    http::header,
    response::{IntoResponse, Response},
};
use regex::{Captures, Regex};
use serde_json::{json, Value};
use std::sync::LazyLock;
use worker::Url;

// discord allows for 10 media gallery items
const MEDIA_GALLERY_ITEM_LIMIT: usize = 10;
// discord allows for 4 images in open graph, although component embeds are used over this now
const OPEN_GRAPH_IMAGE_LIMIT: usize = 4;
// discord drops the component embed when its json is over 3000 bytes
const COMPONENT_EMBED_BYTE_LIMIT: usize = 3000;
// description in the open graph tags
const OPEN_GRAPH_DESCRIPTION_LIMIT: usize = 3500;

type Stat = (&'static str, fn(&Stats) -> Option<u64>);

pub struct Site {
    pub name: &'static str,
    pub accent_color: u32,
    pub stats: &'static [Stat],
    pub mention_url: Option<&'static str>,
    pub hashtag_url: Option<&'static str>,
}

// urls are matched first so @ and # inside them are ignored
static TAG_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"https?://\S+|\B@(?P<mention>[A-Za-z0-9_]+(?:\.[A-Za-z0-9_]+)*)|#(?P<hashtag>\w+)")
        .unwrap()
});

fn link_tags(site: &Site, text: &str) -> String {
    TAG_PATTERN
        .replace_all(text, |captures: &Captures| {
            let tag = captures.get(0).unwrap();
            let url = match (captures.name("mention"), captures.name("hashtag")) {
                (Some(name), _) => site.mention_url.map(|url| (url, name.as_str())),
                // `#1` is a number, not a hashtag
                (_, Some(name)) if !name.as_str().bytes().all(|byte| byte.is_ascii_digit()) => {
                    // an invisible joiner after a hashtag is not part of its name
                    let name = name.as_str().trim_end_matches(['\u{200c}', '\u{200d}']);
                    site.hashtag_url.map(|url| (url, name))
                }
                _ => None,
            };
            // a tag the text was truncated in would link to a different one
            let truncated = &text[tag.end()..] == "...";
            match url
                .filter(|_| !truncated)
                .and_then(|(url, name)| Url::parse(&format!("{url}{name}")).ok())
            {
                Some(url) => format!("[{}]({url})", tag.as_str()),
                None => tag.as_str().to_string(),
            }
        })
        .into_owned()
}

pub fn html_response(body: String) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::VARY, "User-Agent"),
        ],
        body,
    )
        .into_response()
}

pub fn discord_image_post_html(
    site: &Site,
    embed: &EmbedPost,
    media_url: impl Fn(usize) -> String,
    avatar_url: impl Fn(usize) -> String,
) -> String {
    let component_json = |description_limit: usize| {
        let mut components = Vec::new();
        let mut offset = 0;
        for (index, (label, post)) in embed.entries().enumerate() {
            if index > 0 {
                components.push(divider());
            }
            let avatar = post
                .author
                .as_ref()
                .and_then(|author| author.avatar_url.as_ref())
                .map(|_| avatar_url(index));
            components.extend(post_components(
                site,
                label,
                post,
                description_limit,
                |media_index| media_url(offset + media_index),
                avatar.as_deref(),
            ));
            offset += post.media.len();
        }
        json!({
            "component": {
                "type": 17,
                "accent_color": site.accent_color,
                "components": components,
            },
        })
        .to_string()
        .replace('<', "\\u003c")
    };
    // the longest description length, in characters, that keeps the json within the byte limit
    let (mut low, mut high) = (0, COMPONENT_EMBED_BYTE_LIMIT);
    while low < high {
        let middle = (low + high).div_ceil(2);
        if component_json(middle).len() <= COMPONENT_EMBED_BYTE_LIMIT {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    let component = component_json(low);
    let author_name = embed
        .author
        .as_ref()
        .map_or(site.name.to_string(), author_name);
    let meta = post_meta_tags(site, &author_name, embed, media_url);

    format!(
        r#"<!doctype html><html><head><script id="discord:component-embed" type="application/json">{component}</script>{meta}</head><body><a href="{}">{}</a></body></html>"#,
        escape_html(&embed.source_url),
        escape_html(&author_name),
    )
}

fn post_components(
    site: &Site,
    label: Option<&str>,
    post: &EmbedPost,
    description_limit: usize,
    media_url: impl Fn(usize) -> String,
    avatar: Option<&str>,
) -> Vec<Value> {
    // replied to and quoted posts have their text in a quote
    let quoted = |text: String| match label {
        Some(_) => text
            .lines()
            .map(|line| format!("> {line}"))
            .collect::<Vec<_>>()
            .join("\n"),
        None => text,
    };

    let mut components = Vec::new();
    if let Some(translation) = &post.translation {
        components.push(json!({
            "type": 10,
            "content": format!(
                "**Translation ({})**\n{}",
                translation.target_lang,
                quoted(truncate(&translation.text, description_limit))
            ),
        }));
        components.push(divider());
    }

    let text = post
        .description
        .as_deref()
        .filter(|description| !description.is_empty())
        .map(|description| truncate(description, description_limit))
        .map(|text| match label {
            Some(_) => text
                .lines()
                .map(|line| format!("> {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
            None => text,
        });

    let lines: Vec<String> = label
        .map(|label| format!("**{label}**"))
        .into_iter()
        .chain(post.author.as_ref().map(author_line))
        .chain(text)
        .collect();

    let header = (!lines.is_empty()).then(|| json!({ "type": 10, "content": lines.join("\n") }));
    let header = match avatar {
        Some(avatar) => header.map(|header| {
            json!({
                "type": 9,
                "components": [header],
                "accessory": { "type": 11, "media": { "url": avatar } },
            })
        }),
        None => header,
    };
    components.extend(header);

    if !post.media.is_empty() {
        let gallery: Vec<Value> = (0..post.media.len().min(MEDIA_GALLERY_ITEM_LIMIT))
            .map(|index| json!({ "media": { "url": media_url(index) } }))
            .collect();
        components.push(json!({ "type": 12, "items": gallery }));
    }

    let stats = post.stats.clone().unwrap_or_default();
    let footer = site
        .stats
        .iter()
        .filter_map(|(icon, value)| Some(format!("{icon} {}", value(&stats)?)))
        .chain([format!("[View on {}]({})", site.name, post.source_url)])
        .collect::<Vec<_>>()
        .join(" · ");
    components.push(json!({ "type": 10, "content": footer }));
    components
}

fn divider() -> Value {
    json!({ "type": 14, "divider": true, "spacing": 1 })
}

fn author_line(author: &Author) -> String {
    // cant escape markdown (underscores) since discord does not render them correctly in links
    let handle = match &author.profile_url {
        Some(url) => format!("[@{}]({url})", author.username),
        None => format!("@{}", author.username),
    };

    let name = author
        .display_name
        .as_ref()
        .map(|name| name.replace('_', r"\_"));

    match name {
        Some(name) if author.username.is_empty() => format!("**{name}**"),
        Some(name) if author.display_name.as_ref() != Some(&author.username) => {
            format!("**{name} ({handle})**")
        }
        _ => format!("**{handle}**"),
    }
}

fn post_meta_tags(
    site: &Site,
    author_name: &str,
    embed: &EmbedPost,
    media_url: impl Fn(usize) -> String,
) -> String {
    let mut tags = vec![
        meta_property("twitter:card", "summary_large_image"),
        meta_property("og:type", "article"),
        meta_property("og:title", author_name),
        meta_property("twitter:title", author_name),
        meta_property("og:site_name", site.name),
        meta_property("og:url", &embed.source_url),
        meta_name("author", author_name),
        meta_name("theme-color", &format!("#{:06X}", site.accent_color)),
    ];
    if let Some(description) = embed
        .description
        .as_deref()
        .filter(|description| !description.is_empty())
    {
        let description = truncate(description, OPEN_GRAPH_DESCRIPTION_LIMIT);
        tags.push(meta_property("og:description", &description));
        tags.push(meta_property("twitter:description", &description));
    }

    for (index, media) in embed.all_media().enumerate().take(OPEN_GRAPH_IMAGE_LIMIT) {
        let url = media_url(index);
        let kind = match media.kind {
            MediaKind::Video => "video",
            _ => "image",
        };
        tags.push(meta_property(&format!("og:{kind}"), &url));
        tags.push(meta_property(&format!("twitter:{kind}"), &url));
        if let Some(width) = media.width {
            tags.push(meta_property(
                &format!("og:{kind}:width"),
                &width.to_string(),
            ));
            tags.push(meta_property(
                &format!("twitter:{kind}:width"),
                &width.to_string(),
            ));
        }
        if let Some(height) = media.height {
            tags.push(meta_property(
                &format!("og:{kind}:height"),
                &height.to_string(),
            ));
            tags.push(meta_property(
                &format!("twitter:{kind}:height"),
                &height.to_string(),
            ));
        }
    }

    tags.concat()
}

pub fn meta_property(property: &str, content: &str) -> String {
    format!(
        r#"<meta property="{}" content="{}">"#,
        escape_html(property),
        escape_html(content)
    )
}

pub fn meta_name(name: &str, content: &str) -> String {
    format!(
        r#"<meta name="{}" content="{}">"#,
        escape_html(name),
        escape_html(content)
    )
}

pub fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn truncate(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_string();
    }
    format!(
        "{}...",
        value
            .chars()
            .take(limit.saturating_sub(3))
            .collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SITE: Site = Site {
        name: "Example",
        accent_color: 0,
        stats: &[],
        mention_url: None,
        hashtag_url: None,
    };
    const TAGGED_SITE: Site = Site {
        mention_url: Some("https://example.com/@"),
        hashtag_url: Some("https://example.com/tag/"),
        ..SITE
    };

    fn post(description: &str, reply_to: Option<EmbedPost>) -> EmbedPost {
        EmbedPost {
            provider: "example".to_string(),
            provider_id: "1".to_string(),
            source_url: "https://example.com/post/1".to_string(),
            title: None,
            description: Some(description.to_string()),
            author: None,
            media: Vec::new(),
            stats: None,
            lang: None,
            reply_to: reply_to.map(Box::new),
            quote: None,
            translation: None,
        }
    }

    fn component_json(embed: &EmbedPost) -> String {
        let body = discord_image_post_html(&SITE, embed, |_| String::new(), |_| String::new());
        let (_, rest) = body.split_once(r#"type="application/json">"#).unwrap();
        rest.split_once("</script>").unwrap().0.to_string()
    }

    #[test]
    fn long_descriptions_fill_the_component_embed_byte_limit() {
        let reply_to = post(&"é<\n".repeat(2000), None);
        let json = component_json(&post(&"lorem ipsum ".repeat(500), Some(reply_to)));

        assert!(
            (COMPONENT_EMBED_BYTE_LIMIT - 20..=COMPONENT_EMBED_BYTE_LIMIT).contains(&json.len()),
            "{}",
            json.len()
        );
    }

    fn author(username: &str, display_name: &str) -> Author {
        Author {
            id: None,
            username: username.to_string(),
            display_name: Some(display_name.to_string()),
            profile_url: Some(format!("https://example.com/{username}")),
            avatar_url: None,
            verified: None,
        }
    }

    #[test]
    fn links_the_handle_after_the_author_name() {
        assert_eq!(
            author_line(&author("xen_miku", "Xen🐾")),
            "**Xen🐾 ([@xen_miku](https://example.com/xen_miku))**"
        );
    }

    #[test]
    fn links_the_handle_of_authors_without_another_name() {
        assert_eq!(
            author_line(&author("poster", "poster")),
            "**[@poster](https://example.com/poster)**"
        );
    }

    #[test]
    fn links_handles_discord_puts_in_italics() {
        assert_eq!(
            author_line(&author("oz_photo_", "オズ")),
            "**オズ ([@oz_photo_](https://example.com/oz_photo_))**"
        );
    }

    #[test]
    fn escapes_the_underscores_in_an_author_name() {
        assert_eq!(
            author_line(&author("poster", "a_b_")),
            r"**a\_b\_ ([@poster](https://example.com/poster))**"
        );
    }

    #[test]
    fn leaves_authors_without_a_handle_unlinked() {
        assert_eq!(author_line(&author("", "Page Name")), "**Page Name**");
    }

    #[test]
    fn links_mentions_and_hashtags() {
        assert_eq!(
            link_tags(&TAGGED_SITE, "hi @some.one. #fyp#日本 (@other)"),
            "hi [@some.one](https://example.com/@some.one). [#fyp](https://example.com/tag/fyp)[#日本](https://example.com/tag/%E6%97%A5%E6%9C%AC) ([@other](https://example.com/@other))"
        );
    }

    #[test]
    fn links_hashtags_without_their_trailing_joiner() {
        assert_eq!(
            link_tags(&TAGGED_SITE, "#tag\u{200c} next"),
            "[#tag\u{200c}](https://example.com/tag/tag) next"
        );
    }

    #[test]
    fn leaves_emails_urls_numbers_and_cut_off_tags_unlinked() {
        let text = "a@b.co https://example.com/@a#b #1 @cut...";

        assert_eq!(link_tags(&TAGGED_SITE, text), text);
    }

    #[test]
    fn sites_without_tag_urls_keep_their_text() {
        assert_eq!(link_tags(&SITE, "@na_me_ #tag"), "@na_me_ #tag");
    }

    #[test]
    fn short_descriptions_are_not_truncated() {
        let json = component_json(&post("short", None));

        assert!(json.contains(r#""content":"short""#), "{json}");
    }
}
