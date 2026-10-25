use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use tokio::sync::{Mutex, Notify};
use tokio::task;

use crate::analytics::Analytics;
use crate::embeds::VideoKind;
use crate::embeds::cache_manager::VideoCache;
use crate::embeds::image_client::{EmbedImageClient, OgImageError};
use crate::embeds::renderer::{FfmpegRenderer, RenderInput, RendererError};
use crate::embeds::spotify_metadata::{SpotifyMetadataClient, SpotifyMetadataError};
use crate::embeds::video_source::{B2VideoSource, VideoSourceError};

#[derive(Clone, Debug)]
pub struct ServedPreview {
    pub video_bytes: Bytes,
    pub mime: &'static str,
    pub cache_status: CacheStatus,
}

#[derive(Clone, Debug)]
pub enum CacheStatus {
    Hit,
    Hydrated,
    Rendered,
}

#[derive(Clone, Debug)]
pub struct PreloadedPreviewInput {
    pub track_id: String,
    pub video_kind: VideoKind,
    pub preview_url: String,
    pub og_bytes: Bytes,
}

#[derive(Debug, thiserror::Error)]
pub enum PreviewGenerationError {
    #[error("invalid input: {0}")]
    InvalidInput(&'static str),

    #[error("spotify metadata failed: {0}")]
    Metadata(#[from] SpotifyMetadataError),

    #[error("og image generation failed: {0}")]
    Image(#[from] OgImageError),

    #[error("video source failed: {0}")]
    Source(#[from] VideoSourceError),

    #[error("renderer failed: {0}")]
    Renderer(#[from] RendererError),

    #[error("video generation failed")]
    GenerationFailed,
}

pub struct PreviewGeneration {
    cache: Arc<VideoCache>,
    metadata: Arc<SpotifyMetadataClient>,
    image_client: Arc<EmbedImageClient>,
    renderer: Arc<FfmpegRenderer>,
    video_source: Arc<B2VideoSource>,
    analytics: Arc<Analytics>,
    generation_tasks: Arc<Mutex<HashMap<String, Arc<Notify>>>>,
}

impl PreviewGeneration {
    pub fn new(
        cache: Arc<VideoCache>,
        metadata: Arc<SpotifyMetadataClient>,
        image_client: Arc<EmbedImageClient>,
        renderer: Arc<FfmpegRenderer>,
        video_source: Arc<B2VideoSource>,
        analytics: Arc<Analytics>,
    ) -> Self {
        Self {
            cache,
            metadata,
            image_client,
            renderer,
            video_source,
            analytics,
            generation_tasks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn ensure_and_serve(
        &self,
        raw_track_id: &str,
    ) -> Result<ServedPreview, PreviewGenerationError> {
        let track_id = normalize_track_id(raw_track_id)?;
        tracing::debug!(track_id = track_id.as_str(), "ensure_and_serve started");

        if let Some((bytes, video_kind)) = self.cache.get_video_bytes(&track_id).await {
            tracing::debug!(track_id = track_id.as_str(), "preview video cache hit");
            self.analytics
                .video_cache_hit(&track_id, video_kind, bytes.len());
            self.analytics
                .video_served(&track_id, video_kind, bytes.len(), "memory");
            return Ok(ServedPreview {
                video_bytes: bytes,
                mime: "video/mp4",
                cache_status: CacheStatus::Hit,
            });
        }

        if let Some((bytes, video_kind)) = self.hydrate_from_source(&track_id, None).await? {
            tracing::debug!(track_id = track_id.as_str(), "preview hydrated from source");
            self.analytics
                .video_served(&track_id, video_kind, bytes.len(), "source");
            return Ok(ServedPreview {
                video_bytes: bytes,
                mime: "video/mp4",
                cache_status: CacheStatus::Hydrated,
            });
        }

        tracing::debug!(
            track_id = track_id.as_str(),
            "fetching spotify metadata for preview render"
        );
        let spotify_data = self
            .metadata
            .get_track_or_episode_metadata(&track_id)
            .await?;
        tracing::debug!(
            track_id = track_id.as_str(),
            media_id = spotify_data.media_id.as_str(),
            "spotify metadata fetched"
        );

        tracing::debug!(
            track_id = track_id.as_str(),
            "generating OG image for preview render"
        );
        let og = self.image_client.generate_track_og(&spotify_data).await?;

        tracing::debug!(
            track_id = track_id.as_str(),
            "ensuring preview video is generated"
        );
        self.ensure_generated(PreloadedPreviewInput {
            track_id: spotify_data.media_id,
            video_kind: spotify_data.video_kind,
            preview_url: spotify_data.preview_audio_url,
            og_bytes: og.image_bytes,
        })
        .await?;

        let (bytes, video_kind) = self
            .cache
            .get_video_bytes(&track_id)
            .await
            .ok_or(PreviewGenerationError::GenerationFailed)?;

        tracing::debug!(
            track_id = track_id.as_str(),
            "rendered preview available in cache"
        );

        self.analytics
            .video_served(&track_id, video_kind, bytes.len(), "rendered");

        Ok(ServedPreview {
            video_bytes: bytes,
            mime: "video/mp4",
            cache_status: CacheStatus::Rendered,
        })
    }

    pub async fn ensure_generated(
        &self,
        input: PreloadedPreviewInput,
    ) -> Result<CacheStatus, PreviewGenerationError> {
        let track_id = normalize_track_id(&input.track_id)?;
        tracing::debug!(track_id = track_id.as_str(), "ensure_generated started");

        if self.cache.has_id(&track_id).await {
            tracing::debug!(
                track_id = track_id.as_str(),
                "generation skipped, already in cache"
            );
            return Ok(CacheStatus::Hit);
        }

        if self
            .hydrate_from_source(&track_id, Some(input.video_kind))
            .await?
            .is_some()
        {
            tracing::debug!(
                track_id = track_id.as_str(),
                "generation skipped, hydrated from source"
            );
            return Ok(CacheStatus::Hydrated);
        }

        let mut is_leader = false;
        let notify = {
            let mut tasks = self.generation_tasks.lock().await;
            if let Some(existing_notify) = tasks.get(&track_id) {
                Arc::clone(existing_notify)
            } else {
                let notify = Arc::new(Notify::new());
                tasks.insert(track_id.clone(), Arc::clone(&notify));
                is_leader = true;
                notify
            }
        };

        if !is_leader {
            tracing::debug!(
                track_id = track_id.as_str(),
                "waiting for in-flight generation task"
            );
            notify.notified().await;
            if self.cache.has_id(&track_id).await {
                tracing::debug!(
                    track_id = track_id.as_str(),
                    "generation waiter resolved from cache"
                );
                return Ok(CacheStatus::Hit);
            }
            if self
                .hydrate_from_source(&track_id, Some(input.video_kind))
                .await?
                .is_some()
            {
                tracing::debug!(
                    track_id = track_id.as_str(),
                    "generation waiter resolved from source hydration"
                );
                return Ok(CacheStatus::Hydrated);
            }
            return Err(PreviewGenerationError::GenerationFailed);
        }

        tracing::debug!(track_id = track_id.as_str(), "leading generation task");

        let result = self
            .render_and_cache(PreloadedPreviewInput {
                track_id: track_id.clone(),
                video_kind: input.video_kind,
                preview_url: input.preview_url,
                og_bytes: input.og_bytes,
            })
            .await;

        let mut tasks = self.generation_tasks.lock().await;
        if let Some(notify) = tasks.remove(&track_id) {
            notify.notify_waiters();
        }
        drop(tasks);

        result.map(|()| CacheStatus::Rendered)
    }

    pub async fn invalidate_stale_variants(&self, video_id_prefix: &str, keep_suffix: &str) {
        let removed_from_cache = self
            .cache
            .remove_stale_variants(video_id_prefix, keep_suffix)
            .await;
        tracing::info!(
            video_id_prefix = video_id_prefix,
            keep_suffix = keep_suffix,
            removed_from_cache = removed_from_cache,
            "invalidating stale collection videos"
        );

        let video_source = self.video_source.clone();
        let video_id_prefix = video_id_prefix.to_string();
        let keep_suffix = keep_suffix.to_string();
        task::spawn(async move {
            match video_source
                .delete_stale_variants(&video_id_prefix, &keep_suffix)
                .await
            {
                Ok(deleted) => tracing::info!(
                    video_id_prefix = video_id_prefix.as_str(),
                    deleted = deleted,
                    "removed stale collection videos from source"
                ),
                Err(error) => tracing::warn!(
                    video_id_prefix = video_id_prefix.as_str(),
                    error = %error,
                    "failed to remove stale collection videos from source"
                ),
            }
        });
    }

    async fn render_and_cache(
        &self,
        input: PreloadedPreviewInput,
    ) -> Result<(), PreviewGenerationError> {
        let started_at = Instant::now();
        let track_id = input.track_id.clone();
        tracing::info!(track_id = track_id.as_str(), "rendering preview video");

        let rendered_bytes = match self
            .renderer
            .render_preview_video(RenderInput {
                track_id: track_id.clone(),
                preview_url: input.preview_url,
                track_og_bytes: input.og_bytes,
            })
            .await
        {
            Ok(bytes) => bytes,
            Err(error) => {
                self.analytics.video_generation_failed(
                    &track_id,
                    input.video_kind,
                    elapsed_millis(started_at.elapsed()),
                );
                return Err(error.into());
            }
        };

        self.analytics.video_generated(
            &track_id,
            input.video_kind,
            rendered_bytes.len(),
            elapsed_millis(started_at.elapsed()),
        );

        tracing::debug!(
            track_id = track_id.as_str(),
            video_size_bytes = rendered_bytes.len(),
            "preview render complete"
        );

        self.cache
            .cache_video_bytes(track_id.clone(), rendered_bytes.clone(), input.video_kind)
            .await;

        let video_source = self.video_source.clone();
        let renderer = self.renderer.clone();
        task::spawn(async move {
            tracing::debug!(
                track_id = track_id.as_str(),
                "checking remote source for rendered preview"
            );
            if video_source.has_video(&track_id).await.ok() != Some(true) {
                tracing::debug!(
                    track_id = track_id.as_str(),
                    "uploading rendered preview to source"
                );
                let _ = video_source
                    .upload_video_bytes(&track_id, rendered_bytes)
                    .await;
            }
            renderer.remove_track_output(&track_id).await;
            tracing::debug!(
                track_id = track_id.as_str(),
                "removed local renderer output directory"
            );
        });

        Ok(())
    }

    async fn hydrate_from_source(
        &self,
        track_id: &str,
        video_kind: Option<VideoKind>,
    ) -> Result<Option<(Bytes, VideoKind)>, PreviewGenerationError> {
        tracing::debug!(track_id = track_id, "attempting source hydration");
        match self.video_source.fetch_video_bytes(track_id).await {
            Ok(bytes) => {
                tracing::debug!(track_id = track_id, "source hydration hit");
                let video_kind = match video_kind {
                    Some(video_kind) => video_kind,
                    None => {
                        self.metadata
                            .get_track_or_episode_metadata(track_id)
                            .await?
                            .video_kind
                    }
                };
                self.analytics
                    .video_source_hit(track_id, video_kind, bytes.len());
                self.cache
                    .cache_video_bytes(track_id.to_string(), bytes.clone(), video_kind)
                    .await;
                Ok(Some((bytes, video_kind)))
            }
            Err(VideoSourceError::NotFound(_)) => {
                tracing::debug!(track_id = track_id, "source hydration miss");
                Ok(None)
            }
            Err(error) => {
                tracing::warn!(track_id = track_id, "source hydration failed");
                Err(PreviewGenerationError::Source(error))
            }
        }
    }
}

fn elapsed_millis(duration: std::time::Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

pub fn normalize_track_id(raw_track_id: &str) -> Result<String, PreviewGenerationError> {
    let value = raw_track_id
        .strip_suffix(".mp4")
        .unwrap_or(raw_track_id)
        .trim()
        .to_string();

    if value.is_empty() {
        return Err(PreviewGenerationError::InvalidInput("track_id is empty"));
    }

    Ok(value)
}
