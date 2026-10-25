use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::RwLock;

use crate::analytics::Analytics;
use crate::embeds::VideoKind;

pub struct VideoCache {
    cache: BoundedCache<CachedVideoData>,
    analytics: Arc<Analytics>,
}

impl VideoCache {
    pub fn new(max_size_bytes: u64, analytics: Arc<Analytics>) -> VideoCache {
        VideoCache {
            cache: BoundedCache::new(max_size_bytes),
            analytics,
        }
    }

    pub async fn has_id(&self, video_id: &str) -> bool {
        self.cache.contains_key(video_id).await
    }

    pub async fn get_video_bytes(&self, video_id: &str) -> Option<(Bytes, VideoKind)> {
        let cached = self.cache.get(video_id).await?;
        Some((cached.bytes, cached.video_kind))
    }

    pub async fn remove_stale_variants(&self, video_id_prefix: &str, keep_suffix: &str) -> usize {
        self.cache
            .remove_where(|video_id| {
                video_id.starts_with(video_id_prefix) && !video_id.ends_with(keep_suffix)
            })
            .await
    }

    pub async fn cache_video_bytes(
        &self,
        video_id: String,
        video_bytes: Bytes,
        video_kind: VideoKind,
    ) {
        let incoming_size = video_bytes.len() as u64;
        match self
            .cache
            .insert(
                video_id,
                incoming_size,
                CachedVideoData {
                    bytes: video_bytes,
                    video_kind,
                },
            )
            .await
        {
            CacheInsertResult::Disabled => {
                self.analytics
                    .video_cache_write_skipped("disabled", incoming_size, 0);
            }
            CacheInsertResult::Oversized { max_size_bytes } => {
                self.analytics.video_cache_write_skipped(
                    "oversized",
                    incoming_size,
                    max_size_bytes,
                );
            }
            CacheInsertResult::Stored {
                current_size_bytes,
                max_size_bytes,
                entry_count,
                replaced_existing,
                evictions,
            } => {
                self.analytics.video_cache_updated(
                    current_size_bytes,
                    max_size_bytes,
                    entry_count,
                    incoming_size,
                    replaced_existing,
                );
                if evictions > 0 {
                    self.analytics.video_cache_evicted(
                        evictions,
                        current_size_bytes,
                        max_size_bytes,
                        entry_count,
                    );
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct MetadataCache {
    cache: BoundedCache<Bytes>,
}

impl MetadataCache {
    pub fn new(max_size_bytes: u64) -> Self {
        Self {
            cache: BoundedCache::new(max_size_bytes),
        }
    }

    pub async fn get(&self, key: &str) -> Option<Bytes> {
        self.cache.get(key).await
    }

    pub async fn insert(&self, key: String, bytes: Bytes) {
        self.insert_with_ttl(key, bytes, None).await;
    }

    pub async fn insert_with_ttl(&self, key: String, bytes: Bytes, ttl: Option<Duration>) {
        let size = bytes.len() as u64;
        let _ = self.cache.insert_with_ttl(key, size, bytes, ttl).await;
    }
}

#[derive(Clone)]
struct BoundedCache<T> {
    state: Arc<RwLock<CacheState<T>>>,
}

impl<T: Clone> BoundedCache<T> {
    fn new(max_size_bytes: u64) -> Self {
        Self {
            state: Arc::new(RwLock::new(CacheState {
                entries: HashMap::new(),
                current_size_bytes: 0,
                max_size_bytes,
            })),
        }
    }

    async fn contains_key(&self, key: &str) -> bool {
        self.state
            .read()
            .await
            .entries
            .get(key)
            .is_some_and(|cached| !cached.is_expired(Instant::now()))
    }

    async fn get(&self, key: &str) -> Option<T> {
        let mut state = self.state.write().await;
        let now = Instant::now();
        if state
            .entries
            .get(key)
            .is_some_and(|cached| cached.is_expired(now))
        {
            state.remove_entry(key);
            return None;
        }

        let cached = state.entries.get_mut(key)?;
        cached.last_accessed = now;
        Some(cached.value.clone())
    }

    async fn remove_where(&self, predicate: impl Fn(&str) -> bool) -> usize {
        let mut state = self.state.write().await;
        let keys = state
            .entries
            .keys()
            .filter(|key| predicate(key))
            .cloned()
            .collect::<Vec<_>>();
        for key in &keys {
            state.remove_entry(key);
        }
        keys.len()
    }

    async fn insert(&self, key: String, size_bytes: u64, value: T) -> CacheInsertResult {
        self.insert_with_ttl(key, size_bytes, value, None).await
    }

    async fn insert_with_ttl(
        &self,
        key: String,
        size_bytes: u64,
        value: T,
        ttl: Option<Duration>,
    ) -> CacheInsertResult {
        let mut state = self.state.write().await;
        if state.max_size_bytes == 0 {
            return CacheInsertResult::Disabled;
        }
        if size_bytes > state.max_size_bytes {
            return CacheInsertResult::Oversized {
                max_size_bytes: state.max_size_bytes,
            };
        }

        let now = Instant::now();
        let replaced_existing = if let Some(previous) = state.entries.insert(
            key.clone(),
            CachedData {
                value,
                size_bytes,
                last_accessed: now,
                expires_at: ttl.map(|ttl| now + ttl),
            },
        ) {
            state.current_size_bytes -= previous.size_bytes;
            true
        } else {
            false
        };
        state.current_size_bytes += size_bytes;

        let mut evictions = 0_u64;
        if state.current_size_bytes > state.max_size_bytes {
            let mut candidates = state
                .entries
                .iter()
                .filter(|(cached_key, _)| **cached_key != key)
                .map(|(cached_key, entry)| (cached_key.clone(), entry.last_accessed))
                .collect::<Vec<(String, Instant)>>();

            candidates.sort_by_key(|(_, last_accessed)| *last_accessed);

            for (id, _) in candidates {
                if state.current_size_bytes <= state.max_size_bytes {
                    break;
                }

                if let Some(removed) = state.entries.remove(&id) {
                    state.current_size_bytes -= removed.size_bytes;
                    evictions += 1;
                }
            }
        }

        let current_size_bytes = state.current_size_bytes;
        let max_size_bytes = state.max_size_bytes;
        let entry_count = state.entries.len();
        CacheInsertResult::Stored {
            current_size_bytes,
            max_size_bytes,
            entry_count,
            replaced_existing,
            evictions,
        }
    }
}

struct CacheState<T> {
    entries: HashMap<String, CachedData<T>>,
    current_size_bytes: u64,
    max_size_bytes: u64,
}

impl<T> CacheState<T> {
    fn remove_entry(&mut self, key: &str) {
        if let Some(removed) = self.entries.remove(key) {
            self.current_size_bytes -= removed.size_bytes;
        }
    }
}

enum CacheInsertResult {
    Disabled,
    Oversized {
        max_size_bytes: u64,
    },
    Stored {
        current_size_bytes: u64,
        max_size_bytes: u64,
        entry_count: usize,
        replaced_existing: bool,
        evictions: u64,
    },
}

#[derive(Clone, Debug)]
struct CachedVideoData {
    bytes: Bytes,
    video_kind: VideoKind,
}

struct CachedData<T> {
    value: T,
    size_bytes: u64,
    last_accessed: Instant,
    expires_at: Option<Instant>,
}

impl<T> CachedData<T> {
    fn is_expired(&self, now: Instant) -> bool {
        self.expires_at.is_some_and(|expires_at| now >= expires_at)
    }
}

#[cfg(test)]
mod tests {
    use super::{MetadataCache, VideoCache};
    use crate::analytics::Analytics;
    use crate::embeds::VideoKind;
    use bytes::Bytes;
    use std::sync::Arc;
    use tokio::time::{Duration, sleep};

    fn bytes_with_size(size: usize) -> Bytes {
        Bytes::from(vec![0; size])
    }

    async fn test_cache(max_size_bytes: u64) -> VideoCache {
        let analytics = Arc::new(Analytics::new("", "https://us.i.posthog.com").await);
        VideoCache::new(max_size_bytes, analytics)
    }

    #[tokio::test]
    async fn evicts_least_recently_accessed_when_over_capacity() {
        let cache = test_cache(10).await;

        cache
            .cache_video_bytes("a".to_string(), bytes_with_size(4), VideoKind::Track)
            .await;
        sleep(Duration::from_millis(2)).await;
        cache
            .cache_video_bytes("b".to_string(), bytes_with_size(4), VideoKind::Track)
            .await;
        sleep(Duration::from_millis(2)).await;

        let _ = cache.get_video_bytes("a").await;
        sleep(Duration::from_millis(2)).await;

        cache
            .cache_video_bytes("c".to_string(), bytes_with_size(4), VideoKind::Track)
            .await;

        assert!(cache.has_id("a").await);
        assert!(!cache.has_id("b").await);
        assert!(cache.has_id("c").await);
    }

    #[tokio::test]
    async fn skips_oversized_entries() {
        let cache = test_cache(10).await;

        cache
            .cache_video_bytes("large".to_string(), bytes_with_size(11), VideoKind::Track)
            .await;

        assert!(!cache.has_id("large").await);
        assert!(cache.get_video_bytes("large").await.is_none());
    }

    #[tokio::test]
    async fn disabled_cache_always_misses_and_writes_noop() {
        let cache = test_cache(0).await;

        cache
            .cache_video_bytes("a".to_string(), bytes_with_size(4), VideoKind::Track)
            .await;

        assert!(!cache.has_id("a").await);
        assert!(cache.get_video_bytes("a").await.is_none());
    }

    #[tokio::test]
    async fn upsert_replaces_and_keeps_new_entry_under_pressure() {
        let cache = test_cache(10).await;

        cache
            .cache_video_bytes("a".to_string(), bytes_with_size(4), VideoKind::Track)
            .await;
        sleep(Duration::from_millis(2)).await;
        cache
            .cache_video_bytes("b".to_string(), bytes_with_size(4), VideoKind::Track)
            .await;
        sleep(Duration::from_millis(2)).await;

        cache
            .cache_video_bytes("a".to_string(), bytes_with_size(7), VideoKind::Episode)
            .await;

        assert!(cache.has_id("a").await);
        assert!(!cache.has_id("b").await);
        let (bytes, video_kind) = cache.get_video_bytes("a").await.unwrap();
        assert_eq!(bytes.len(), 7);
        assert_eq!(video_kind, VideoKind::Episode);
    }

    #[tokio::test]
    async fn metadata_cache_uses_the_same_lru_eviction() {
        let cache = MetadataCache::new(10);
        cache.insert("a".to_string(), bytes_with_size(4)).await;
        sleep(Duration::from_millis(2)).await;
        cache.insert("b".to_string(), bytes_with_size(4)).await;
        sleep(Duration::from_millis(2)).await;
        let _ = cache.get("a").await;
        sleep(Duration::from_millis(2)).await;

        cache.insert("c".to_string(), bytes_with_size(4)).await;

        assert!(cache.get("a").await.is_some());
        assert!(cache.get("b").await.is_none());
        assert!(cache.get("c").await.is_some());
    }

    #[tokio::test]
    async fn disabled_metadata_cache_always_misses() {
        let cache = MetadataCache::new(0);
        cache.insert("a".to_string(), bytes_with_size(4)).await;

        assert!(cache.get("a").await.is_none());
    }

    #[tokio::test]
    async fn metadata_cache_entries_expire_after_ttl() {
        let cache = MetadataCache::new(1_024);
        cache
            .insert_with_ttl(
                "playlist".to_string(),
                bytes_with_size(4),
                Some(Duration::from_millis(20)),
            )
            .await;
        cache
            .insert_with_ttl("track".to_string(), bytes_with_size(4), None)
            .await;

        assert!(cache.get("playlist").await.is_some());
        sleep(Duration::from_millis(30)).await;

        assert!(cache.get("playlist").await.is_none());
        assert!(cache.get("track").await.is_some());
    }

    #[tokio::test]
    async fn expired_entries_release_their_size_budget() {
        let cache = MetadataCache::new(8);
        cache
            .insert_with_ttl(
                "old".to_string(),
                bytes_with_size(6),
                Some(Duration::from_millis(5)),
            )
            .await;
        sleep(Duration::from_millis(10)).await;
        assert!(cache.get("old").await.is_none());

        cache.insert("new".to_string(), bytes_with_size(6)).await;
        assert!(cache.get("new").await.is_some());
    }

    #[tokio::test]
    async fn remove_stale_variants_keeps_current_fingerprint_only() {
        let cache = test_cache(100).await;
        for id in [
            "playlist-p1-0-t1-old",
            "playlist-p1-1-t2-old",
            "playlist-p1-0-t1-new",
            "playlist-p2-0-t1-old",
            "album-p1-0-t1-old",
        ] {
            cache
                .cache_video_bytes(id.to_string(), bytes_with_size(4), VideoKind::Playlist)
                .await;
        }

        let removed = cache.remove_stale_variants("playlist-p1-", "-new").await;

        assert_eq!(removed, 2);
        assert!(!cache.has_id("playlist-p1-0-t1-old").await);
        assert!(!cache.has_id("playlist-p1-1-t2-old").await);
        assert!(cache.has_id("playlist-p1-0-t1-new").await);
        assert!(cache.has_id("playlist-p2-0-t1-old").await);
        assert!(cache.has_id("album-p1-0-t1-old").await);
    }
}
