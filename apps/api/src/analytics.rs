use axum::http::StatusCode;
use common::{EventName, consumer};
use posthog_rs::{Client, Event};
use serde_json::{Value, json};

use crate::embeds::VideoKind;
use crate::embeds::preview_generation::CacheStatus;

const PLATFORM: &str = "spotify";

pub struct Analytics {
    client: Client,
}

impl Analytics {
    pub async fn new(api_key: &str, host: &str) -> Self {
        let client = posthog_rs::client((api_key, host)).await;

        if api_key.trim().is_empty() {
            tracing::info!("posthog analytics disabled (POSTHOG_API_KEY is empty)");
        } else {
            tracing::info!(posthog_host = host, "posthog analytics enabled");
        }

        Self { client }
    }

    pub fn link_clicked(&self) {
        self.capture(EventName::LinkClicked, json!({ "platform": PLATFORM }));
    }

    pub fn embed_requested(
        &self,
        media_type: &str,
        user_agent: &str,
        host: &str,
        cache: Option<&CacheStatus>,
        status: StatusCode,
        duration_ms: u64,
    ) {
        self.capture(
            EventName::EmbedRequested,
            embed_properties(media_type, user_agent, host, cache, status, duration_ms),
        );
    }

    pub fn video_rendered(&self, kind: VideoKind, size_bytes: Option<usize>, duration_ms: u64) {
        self.capture(
            EventName::VideoRendered,
            render_properties(kind, size_bytes, duration_ms),
        );
    }

    pub fn video_requested(
        &self,
        served: Option<&(VideoKind, CacheStatus)>,
        status: StatusCode,
        duration_ms: u64,
    ) {
        self.capture(
            EventName::VideoRequested,
            video_properties(served, status, duration_ms),
        );
    }

    fn capture(&self, name: EventName, properties: Value) {
        let mut event = Event::new_anon(name.as_str());
        for (key, value) in properties.as_object().into_iter().flatten() {
            if let Err(error) = event.insert_prop(key, value) {
                tracing::warn!(property = key, %error, "failed to add PostHog event property");
            }
        }
        self.client.capture(event);
    }

    pub fn video_cache_updated(
        &self,
        current_size_bytes: u64,
        max_size_bytes: u64,
        entry_count: usize,
        incoming_size_bytes: u64,
        replaced_existing: bool,
    ) {
        self.capture(
            EventName::VideoCacheUpdated,
            json!({
                "current_size_bytes": current_size_bytes,
                "max_size_bytes": max_size_bytes,
                "entry_count": entry_count,
                "incoming_size_bytes": incoming_size_bytes,
                "replaced_existing": replaced_existing,
            }),
        );
    }

    pub fn video_cache_evicted(
        &self,
        eviction_count: u64,
        current_size_bytes: u64,
        max_size_bytes: u64,
        entry_count: usize,
    ) {
        self.capture(
            EventName::VideoCacheEvicted,
            json!({
                "eviction_count": eviction_count,
                "current_size_bytes": current_size_bytes,
                "max_size_bytes": max_size_bytes,
                "entry_count": entry_count,
            }),
        );
    }

    pub fn video_cache_write_skipped(
        &self,
        reason: &str,
        incoming_size_bytes: u64,
        max_size_bytes: u64,
    ) {
        self.capture(
            EventName::VideoCacheWriteSkipped,
            json!({
                "reason": reason,
                "incoming_size_bytes": incoming_size_bytes,
                "max_size_bytes": max_size_bytes,
            }),
        );
    }

    pub async fn shutdown(&self) {
        self.client.shutdown().await;
    }
}

fn embed_properties(
    media_type: &str,
    user_agent: &str,
    host: &str,
    cache: Option<&CacheStatus>,
    status: StatusCode,
    duration_ms: u64,
) -> Value {
    let failed = status.is_client_error() || status.is_server_error();
    let mut properties = json!({
        "platform": PLATFORM,
        "endpoint": "embed",
        "consumer": consumer(user_agent),
        "link_kind": "canonical",
        "media_type": media_type,
        "host": host,
        "status": status.as_u16(),
        "outcome": outcome(failed),
        "duration_ms": duration_ms,
    });

    if let Some(cache) = cache {
        properties["cache"] = match cache {
            CacheStatus::Rendered => "miss",
            CacheStatus::Hit | CacheStatus::Hydrated => "hit",
        }
        .into();
    }

    if !failed {
        properties["media_count"] = 1.into();
        properties["media_kinds"] = json!(["video"]);
        properties["video_source"] = video_source(cache).into();
    }

    properties
}

fn video_source(cache: Option<&CacheStatus>) -> &'static str {
    match cache {
        Some(CacheStatus::Rendered) => "rendered",
        Some(CacheStatus::Hit) => "memory",
        Some(CacheStatus::Hydrated) => "storage",
        // native is a link directly from spotify, such as music video previews
        None => "native",
    }
}

fn outcome(failed: bool) -> &'static str {
    if failed { "error" } else { "ok" }
}

fn render_properties(kind: VideoKind, size_bytes: Option<usize>, duration_ms: u64) -> Value {
    let mut properties = json!({
        "media_type": kind.as_str(),
        "outcome": outcome(size_bytes.is_none()),
        "duration_ms": duration_ms,
    });

    if let Some(size_bytes) = size_bytes {
        properties["size_bytes"] = size_bytes.into();
    }

    properties
}

fn video_properties(
    served: Option<&(VideoKind, CacheStatus)>,
    status: StatusCode,
    duration_ms: u64,
) -> Value {
    let mut properties = json!({
        "status": status.as_u16(),
        "outcome": outcome(served.is_none()),
        "duration_ms": duration_ms,
    });

    if let Some((kind, cache)) = served {
        properties["media_type"] = kind.as_str().into();
        properties["video_source"] = video_source(Some(cache)).into();
    }

    properties
}
