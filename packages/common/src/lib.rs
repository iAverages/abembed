use http::{header::HeaderName, HeaderMap};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventName {
    EmbedRequested,
    LinkClicked,
    VideoRendered,
    VideoRequested,
    VideoCacheUpdated,
    VideoCacheEvicted,
    VideoCacheWriteSkipped,
}

impl EventName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EmbedRequested => "embed requested",
            Self::LinkClicked => "link clicked",
            Self::VideoRendered => "video rendered",
            Self::VideoRequested => "video requested",
            Self::VideoCacheUpdated => "video cache updated",
            Self::VideoCacheEvicted => "video cache evicted",
            Self::VideoCacheWriteSkipped => "video cache write skipped",
        }
    }
}

pub fn header_str(headers: &HeaderMap, name: HeaderName) -> &str {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

pub fn is_browser_navigation(headers: &HeaderMap) -> bool {
    headers
        .get("sec-fetch-mode")
        .is_some_and(|mode| mode == "navigate")
}

pub fn consumer(user_agent: &str) -> &'static str {
    let user_agent = user_agent.to_ascii_lowercase();

    [
        ("discordbot", "discord"),
        ("telegrambot", "telegram"),
        ("slackbot", "slack"),
        ("whatsapp", "whatsapp"),
        ("twitterbot", "twitter"),
        ("facebookexternalhit", "facebook"),
    ]
    .into_iter()
    .find(|(marker, _)| user_agent.contains(marker))
    .map_or("other", |(_, consumer)| consumer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(name, value)| (name.parse().unwrap(), value.parse().unwrap()))
            .collect()
    }

    #[test]
    fn browser_navigation_is_detected() {
        assert!(is_browser_navigation(&headers(&[
            ("user-agent", "Mozilla/5.0 Chrome/136.0.0.0"),
            ("accept", "text/html"),
            ("sec-fetch-mode", "navigate"),
        ])));
    }

    #[test]
    fn discordbot_is_not_a_browser_navigation() {
        assert!(!is_browser_navigation(&headers(&[
            (
                "user-agent",
                "Mozilla/5.0 (compatible; Discordbot/2.0; +https://discordapp.com)"
            ),
            ("accept", "text/html"),
        ])));
    }

    #[test]
    fn unfurlers_are_told_apart_by_user_agent() {
        assert_eq!(
            consumer("Mozilla/5.0 (compatible; Discordbot/2.0; +https://discordapp.com)"),
            "discord"
        );
        assert_eq!(consumer("TelegramBot (like TwitterBot)"), "telegram");
        assert_eq!(consumer("curl/8.5.0"), "other");
        assert_eq!(consumer(""), "other");
    }
}
