use std::io::Cursor;
use std::num::NonZeroU32;

use backblaze_b2_client::definitions::bodies::B2DeleteFileVersionBody;
use backblaze_b2_client::definitions::query_params::{
    B2DownloadFileQueryParameters, B2ListFileNamesQueryParameters,
};
use bytes::Bytes;
use futures::stream::StreamExt;

use crate::{MACHINA_CONFIG, get_b2};

#[derive(Debug, thiserror::Error)]
pub enum VideoSourceError {
    #[error("video with id {0} was not found")]
    NotFound(String),

    #[error("failed to read video bytes from source")]
    ReadFailed,

    #[error("failed to upload video bytes to source")]
    UploadFailed,

    #[error("failed to delete video from source")]
    DeleteFailed,
}

pub struct B2VideoSource;

impl B2VideoSource {
    pub fn new() -> Self {
        Self
    }

    pub async fn has_video(&self, track_id: &str) -> Result<bool, VideoSourceError> {
        let files = get_b2()
            .basic_client()
            .list_file_names(
                B2ListFileNamesQueryParameters::builder()
                    .bucket_id(MACHINA_CONFIG.b2_bucket_id.clone())
                    .prefix(Some(get_b2_video_path(track_id)))
                    .build(),
            )
            .await
            .map_err(|_| VideoSourceError::ReadFailed)?;

        Ok(!files.files.is_empty())
    }

    pub async fn fetch_video_bytes(&self, track_id: &str) -> Result<Bytes, VideoSourceError> {
        let files = get_b2()
            .basic_client()
            .list_file_names(
                B2ListFileNamesQueryParameters::builder()
                    .bucket_id(MACHINA_CONFIG.b2_bucket_id.clone())
                    .prefix(Some(get_b2_video_path(track_id)))
                    .build(),
            )
            .await
            .map_err(|_| VideoSourceError::ReadFailed)?;

        let Some(file) = files.files.first() else {
            return Err(VideoSourceError::NotFound(track_id.to_string()));
        };

        let video = get_b2()
            .basic_client()
            .download_file_by_id(
                file.file_id.clone(),
                Some(B2DownloadFileQueryParameters::builder().build()),
            )
            .await
            .map_err(|_| VideoSourceError::ReadFailed)?;
        let (size, mut stream) = video.file.into_stream();
        let mut buffer: Vec<u8> = Vec::with_capacity(size);

        while let Some(value) = stream.next().await {
            let chunk = value.map_err(|_| VideoSourceError::ReadFailed)?;
            buffer.extend_from_slice(chunk.as_ref());
        }

        Ok(Bytes::from(buffer))
    }

    pub async fn upload_video_bytes(
        &self,
        track_id: &str,
        video_bytes: Bytes,
    ) -> Result<(), VideoSourceError> {
        let length = video_bytes.len() as u64;
        let wrapped = Cursor::new(video_bytes);

        let upload = get_b2()
            .create_upload(
                wrapped,
                get_b2_video_path(track_id),
                MACHINA_CONFIG.b2_bucket_id.clone(),
                None,
                length,
                None,
            )
            .await;

        upload
            .start()
            .await
            .map_err(|_| VideoSourceError::UploadFailed)?;
        Ok(())
    }

    pub async fn delete_stale_variants(
        &self,
        video_id_prefix: &str,
        keep_suffix: &str,
    ) -> Result<usize, VideoSourceError> {
        let files = get_b2()
            .basic_client()
            .list_file_names(
                B2ListFileNamesQueryParameters::builder()
                    .bucket_id(MACHINA_CONFIG.b2_bucket_id.clone())
                    .prefix(Some(format!("{B2_VIDEO_ROOT}/{video_id_prefix}")))
                    .max_file_count(NonZeroU32::new(1_000))
                    .build(),
            )
            .await
            .map_err(|_| VideoSourceError::ReadFailed)?;

        let mut deleted = 0;
        for file in files.files {
            let Some(video_id) = video_id_from_b2_path(&file.file_name) else {
                continue;
            };
            if !is_stale_variant(video_id, video_id_prefix, keep_suffix) {
                continue;
            }

            get_b2()
                .basic_client()
                .delete_file_version(
                    B2DeleteFileVersionBody::builder()
                        .file_name(file.file_name.clone())
                        .file_id(file.file_id.clone())
                        .build(),
                )
                .await
                .map_err(|_| VideoSourceError::DeleteFailed)?;
            tracing::debug!(video_id = video_id, "deleted stale video from source");
            deleted += 1;
        }

        Ok(deleted)
    }
}

const B2_VIDEO_ROOT: &str = "generated";

fn get_b2_video_path(track_id: &str) -> String {
    format!("{B2_VIDEO_ROOT}/{track_id}/out.mp4")
}

fn video_id_from_b2_path(path: &str) -> Option<&str> {
    path.strip_prefix(B2_VIDEO_ROOT)?
        .strip_prefix('/')?
        .strip_suffix("/out.mp4")
}

fn is_stale_variant(video_id: &str, video_id_prefix: &str, keep_suffix: &str) -> bool {
    video_id.starts_with(video_id_prefix) && !video_id.ends_with(keep_suffix)
}

#[cfg(test)]
mod tests {
    use super::{get_b2_video_path, is_stale_variant, video_id_from_b2_path};

    #[test]
    fn video_id_round_trips_through_b2_path() {
        let path = get_b2_video_path("playlist-p1-0-t1-abc");
        assert_eq!(path, "generated/playlist-p1-0-t1-abc/out.mp4");
        assert_eq!(video_id_from_b2_path(&path), Some("playlist-p1-0-t1-abc"));
        assert_eq!(video_id_from_b2_path("generated/playlist-p1/og.png"), None);
    }

    #[test]
    fn stale_variant_detection_matches_prefix_and_fingerprint() {
        assert!(is_stale_variant(
            "playlist-p1-0-t1-old",
            "playlist-p1-",
            "-new"
        ));
        assert!(!is_stale_variant(
            "playlist-p1-3-t9-new",
            "playlist-p1-",
            "-new"
        ));
        assert!(!is_stale_variant(
            "playlist-p10-0-t1-old",
            "playlist-p1-",
            "-new"
        ));
        assert!(!is_stale_variant(
            "album-p1-0-t1-old",
            "playlist-p1-",
            "-new"
        ));
    }
}
