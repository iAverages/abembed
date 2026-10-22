use super::{author_name, EmbedPost, MediaKind};
use axum::{
    http::header,
    response::{IntoResponse, Response},
};
use serde_json::{json, Value};

// discord allows for 10 media gallery items
const MEDIA_GALLERY_ITEM_LIMIT: usize = 10;
// discord allows for 4 images in open graph, although component embeds are used over this now
const OPEN_GRAPH_IMAGE_LIMIT: usize = 4;
const DESCRIPTION_LIMIT: usize = 500;

pub struct Site {
    pub name: &'static str,
    pub accent_color: u32,
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
    avatar_url: &str,
) -> String {
    let author_name = embed
        .author
        .as_ref()
        .map_or(site.name.to_string(), author_name);
    let text = embed
        .description
        .as_deref()
        .filter(|description| !description.is_empty())
        .map(|description| truncate(description, DESCRIPTION_LIMIT));
    let content = match (&embed.author, text) {
        (Some(_), Some(text)) => Some(format!("**{author_name}**\n{text}")),
        (Some(_), None) => Some(format!("**{author_name}**")),
        (None, text) => text,
    };

    let header = content.map(|content| json!({ "type": 10, "content": content }));
    let header = match embed
        .author
        .as_ref()
        .and_then(|author| author.avatar_url.as_ref())
    {
        Some(_) => header.map(|header| {
            json!({
                "type": 9,
                "components": [header],
                "accessory": { "type": 11, "media": { "url": avatar_url } },
            })
        }),
        None => header,
    };
    let gallery: Vec<Value> = (0..embed.media.len().min(MEDIA_GALLERY_ITEM_LIMIT))
        .map(|index| json!({ "media": { "url": media_url(index) } }))
        .collect();
    let stats = embed.stats.clone().unwrap_or_default();
    let stats = [
        ("❤️", stats.likes),
        ("🔁", stats.shares),
        ("💬", stats.comments),
    ]
    .iter()
    .filter_map(|(icon, value)| Some(format!("{icon} {}", (*value)?)))
    .collect::<Vec<_>>()
    .join("   ");
    let link = format!("[View on {}]({})", site.name, embed.source_url);
    let footer = if stats.is_empty() {
        link
    } else {
        format!("{stats}\n{link}")
    };

    let component = json!({
        "component": {
            "type": 17,
            "accent_color": site.accent_color,
            "components": header
                .into_iter()
                .chain([
                    json!({ "type": 12, "items": gallery }),
                    json!({ "type": 10, "content": footer }),
                ])
                .collect::<Vec<_>>(),
        },
    })
    .to_string()
    .replace('<', "\\u003c");
    let meta = post_meta_tags(site, &author_name, &embed.source_url, embed, media_url);

    format!(
        r#"<!doctype html><html><head><script id="discord:component-embed" type="application/json">{component}</script>{meta}</head><body><a href="{}">{}</a></body></html>"#,
        escape_html(&embed.source_url),
        escape_html(&author_name),
    )
}

fn post_meta_tags(
    site: &Site,
    author_name: &str,
    url: &str,
    embed: &EmbedPost,
    media_url: impl Fn(usize) -> String,
) -> String {
    let mut tags = vec![
        meta_property("twitter:card", "summary_large_image"),
        meta_property("og:type", "article"),
        meta_property("og:title", author_name),
        meta_property("twitter:title", author_name),
        meta_property("og:site_name", site.name),
        meta_property("og:url", url),
        meta_name("author", author_name),
        meta_name("theme-color", &format!("#{:06X}", site.accent_color)),
    ];

    for (index, media) in embed.media.iter().enumerate().take(OPEN_GRAPH_IMAGE_LIMIT) {
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
    format!("{}...", value.chars().take(limit - 3).collect::<String>())
}
