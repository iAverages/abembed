use std::time::Duration;

use anyhow::{Result, anyhow};
use bytes::Bytes;
use scraper::{Html, Selector};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};

use crate::embeds::VideoKind;
use crate::embeds::cache_manager::MetadataCache;

const SPOTIFY_EMBED_TRACK_LIMIT: usize = 100;
const SPOTIFY_TRACK_QUERY_HASH: &str =
    "ef7026cc85d339320860164956786c652b656365b46931e765290dac9a46f00b";

#[derive(Debug, thiserror::Error)]
pub enum SpotifyMetadataError {
    #[error("failed to fetch spotify metadata")]
    RequestFailed,

    #[error("spotify metadata missing required fields")]
    MissingData,
}

#[derive(Clone, Debug)]
pub struct SpotifyPreviewMetadata {
    pub media_id: String,
    pub video_kind: VideoKind,
    pub song_name: String,
    pub artist_names: Vec<String>,
    pub preview_audio_url: String,
    pub preview_video: Option<SpotifyMusicVideoPreview>,
    pub album_art_url: String,
    pub artist_image_url: Option<String>,
    pub duration_ms: Option<i64>,
    pub release_date: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SpotifyMusicVideoPreview {
    pub video_url: String,
    pub thumbnail_url: String,
}

impl SpotifyPreviewMetadata {
    pub fn artist_text(&self) -> String {
        self.artist_names.join(", ")
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SpotifyCollectionKind {
    Album,
    Playlist,
}

impl SpotifyCollectionKind {
    pub fn path_segment(self) -> &'static str {
        match self {
            Self::Album => "album",
            Self::Playlist => "playlist",
        }
    }

    pub fn video_kind(self) -> VideoKind {
        match self {
            Self::Album => VideoKind::Album,
            Self::Playlist => VideoKind::Playlist,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SpotifyCollectionTrackMetadata {
    pub collection_name: String,
    pub collection_creator: String,
    pub artwork_url: String,
    pub selected_track_index: usize,
    pub track_names: Vec<String>,
    pub track: SpotifyPreviewMetadata,
    // hash of all variables in the collection. mainly for playlists that can be updated by users used to regenerate videos if altered.
    pub content_fingerprint: String,
}

impl SpotifyCollectionTrackMetadata {
    pub fn track_count_is_capped(&self) -> bool {
        self.track_names.len() == SPOTIFY_EMBED_TRACK_LIMIT
    }
}

pub struct SpotifyMetadataClient {
    cache: MetadataCache,
    playlist_metadata_ttl: Duration,
}

impl SpotifyMetadataClient {
    pub fn new(cache: MetadataCache, playlist_metadata_ttl: Duration) -> Self {
        Self {
            cache,
            playlist_metadata_ttl,
        }
    }

    pub async fn get_track_metadata(
        &self,
        track_id: &str,
    ) -> Result<SpotifyPreviewMetadata, SpotifyMetadataError> {
        tracing::debug!(track_id = track_id, "fetching spotify track metadata");
        let json: TrackRoot = self
            .fetch_spotify_embed_json(format!("https://open.spotify.com/embed/track/{track_id}"))
            .await
            .map_err(|_| SpotifyMetadataError::RequestFailed)?;

        let artist_id = json
            .props
            .page_props
            .state
            .data
            .entity
            .artists
            .first()
            .and_then(|artist| parse_media_id_from_uri(&artist.uri));
        let access_token = json
            .props
            .page_props
            .state
            .settings
            .as_ref()
            .map(|settings| settings.session.access_token.clone());
        let mut metadata = normalize_track_metadata(track_id, json)
            .map_err(|_| SpotifyMetadataError::MissingData)?;

        if let Some(artist_id) = artist_id {
            match self.fetch_artist_image(&artist_id).await {
                Ok(image_url) => metadata.artist_image_url = Some(image_url),
                Err(error) => tracing::warn!(
                    artist_id = artist_id,
                    error = %error,
                    "spotify artist image lookup failed"
                ),
            }
        }

        if metadata.preview_video.is_none()
            && let Some(access_token) = access_token
        {
            match self
                .fetch_music_video_preview(track_id, &access_token)
                .await
            {
                Ok(preview_video) => metadata.preview_video = preview_video,
                Err(error) => tracing::warn!(
                    track_id = track_id,
                    error = %error,
                    "spotify music video lookup failed"
                ),
            }
        }

        tracing::debug!(track_id = track_id, "spotify track metadata fetched");
        Ok(metadata)
    }

    pub async fn get_episode_metadata(
        &self,
        episode_id: &str,
    ) -> Result<SpotifyPreviewMetadata, SpotifyMetadataError> {
        tracing::debug!(episode_id = episode_id, "fetching spotify episode metadata");
        let json: EpisodeRoot = self
            .fetch_spotify_embed_json(format!(
                "https://open.spotify.com/embed/episode/{episode_id}"
            ))
            .await
            .map_err(|_| SpotifyMetadataError::RequestFailed)?;

        let metadata = normalize_episode_metadata(episode_id, json)
            .map_err(|_| SpotifyMetadataError::MissingData)?;
        tracing::debug!(episode_id = episode_id, "spotify episode metadata fetched");
        Ok(metadata)
    }

    pub async fn get_track_or_episode_metadata(
        &self,
        media_id: &str,
    ) -> Result<SpotifyPreviewMetadata, SpotifyMetadataError> {
        tracing::debug!(media_id = media_id, "resolving spotify media metadata");
        if let Ok(track) = self.get_track_metadata(media_id).await {
            tracing::debug!(media_id = media_id, "resolved spotify media as track");
            return Ok(track);
        }

        tracing::debug!(
            media_id = media_id,
            "falling back to spotify episode metadata"
        );
        self.get_episode_metadata(media_id).await
    }

    pub async fn get_collection_track_metadata(
        &self,
        collection_kind: SpotifyCollectionKind,
        collection_id: &str,
        raw_track: Option<&str>,
    ) -> Result<SpotifyCollectionTrackMetadata, SpotifyMetadataError> {
        tracing::debug!(
            collection_kind = collection_kind.path_segment(),
            collection_id = collection_id,
            requested_track = raw_track.unwrap_or(""),
            "fetching spotify collection metadata"
        );
        let cache_ttl = match collection_kind {
            SpotifyCollectionKind::Playlist => Some(self.playlist_metadata_ttl),
            SpotifyCollectionKind::Album => None,
        };
        let json: CollectionRoot = self
            .fetch_spotify_embed_json_with_ttl(
                format!(
                    "https://open.spotify.com/embed/{}/{collection_id}",
                    collection_kind.path_segment()
                ),
                cache_ttl,
            )
            .await
            .map_err(|_| SpotifyMetadataError::RequestFailed)?;

        let artist_id = if collection_kind == SpotifyCollectionKind::Album {
            json.props
                .page_props
                .state
                .data
                .entity
                .related_entity_uri
                .as_deref()
                .and_then(parse_media_id_from_uri)
        } else {
            None
        };
        let mut metadata = normalize_collection_metadata(raw_track, json)
            .map_err(|_| SpotifyMetadataError::MissingData)?;

        if let Some(artist_id) = artist_id {
            match self.fetch_artist_image(&artist_id).await {
                Ok(image_url) => metadata.track.artist_image_url = Some(image_url),
                Err(error) => tracing::warn!(
                    artist_id = artist_id,
                    error = %error,
                    "spotify artist image lookup failed"
                ),
            }
        }

        tracing::debug!(
            collection_kind = collection_kind.path_segment(),
            collection_id = collection_id,
            selected_track_index = metadata.selected_track_index,
            "spotify collection metadata fetched"
        );
        Ok(metadata)
    }

    async fn fetch_spotify_embed_json<T: DeserializeOwned>(&self, url: String) -> Result<T> {
        self.fetch_spotify_embed_json_with_ttl(url, None).await
    }

    async fn fetch_spotify_embed_json_with_ttl<T: DeserializeOwned>(
        &self,
        url: String,
        cache_ttl: Option<Duration>,
    ) -> Result<T> {
        let cache_key = format!("embed:{url}");
        if let Some(json) = self.cache.get(&cache_key).await {
            tracing::debug!(url = url.as_str(), "spotify embed cache hit");
            return serde_json::from_slice(&json).map_err(Into::into);
        }

        tracing::debug!(url = url.as_str(), "requesting spotify embed page");
        let response = reqwest::get(url).await?.error_for_status()?;
        tracing::debug!(
            status = response.status().as_u16(),
            "spotify embed response received"
        );
        let html_content = response.text().await?;
        tracing::debug!(html_len = html_content.len(), "spotify embed html loaded");
        let json_text = extract_spotify_next_data_json(&html_content)?;
        tracing::debug!(json_len = json_text.len(), "spotify embed json extracted");
        let json = Bytes::from(json_text);
        let parsed = serde_json::from_slice(&json)?;
        self.cache.insert_with_ttl(cache_key, json, cache_ttl).await;
        Ok(parsed)
    }

    async fn fetch_artist_image(&self, artist_id: &str) -> Result<String> {
        let json: serde_json::Value = self
            .fetch_spotify_embed_json(format!("https://open.spotify.com/embed/artist/{artist_id}"))
            .await?;

        json.pointer("/props/pageProps/state/data/entity/visualIdentity/image")
            .and_then(serde_json::Value::as_array)
            .and_then(|images| {
                images
                    .iter()
                    .filter_map(|image| {
                        Some((
                            image.get("maxWidth")?.as_i64()?,
                            image.get("url")?.as_str()?.trim(),
                        ))
                    })
                    .filter(|(_, url)| !url.is_empty())
                    .max_by_key(|(width, _)| *width)
            })
            .map(|(_, url)| url.to_string())
            .ok_or_else(|| anyhow!("missing artist image"))
    }

    async fn fetch_music_video_preview(
        &self,
        track_id: &str,
        access_token: &str,
    ) -> Result<Option<SpotifyMusicVideoPreview>> {
        let cache_key = format!("partner:{track_id}");
        let response = if let Some(response) = self.cache.get(&cache_key).await {
            tracing::debug!(track_id = track_id, "spotify partner cache hit");
            serde_json::from_slice(&response)?
        } else {
            let response = reqwest::Client::new()
                .post("https://api-partner.spotify.com/pathfinder/v2/query")
                .bearer_auth(access_token)
                .json(&serde_json::json!({
                    "operationName": "queryTrack",
                    "variables": {
                        "uri": format!("spotify:track:{track_id}"),
                        "includeVideoAssociations": true
                    },
                    "extensions": {
                        "persistedQuery": {
                            "version": 1,
                            "sha256Hash": SPOTIFY_TRACK_QUERY_HASH
                        }
                    }
                }))
                .send()
                .await?
                .error_for_status()?
                .bytes()
                .await?;
            let parsed = serde_json::from_slice(&response)?;
            self.cache.insert(cache_key, response).await;
            parsed
        };

        let Some(video_track_id) = parse_music_video_track_id(&response) else {
            return Ok(None);
        };
        let video_root = self
            .fetch_spotify_embed_json(format!(
                "https://open.spotify.com/embed/track/{video_track_id}"
            ))
            .await?;

        Ok(normalize_track_metadata(&video_track_id, video_root)?.preview_video)
    }
}

fn normalize_collection_metadata(
    raw_track: Option<&str>,
    root: CollectionRoot,
) -> Result<SpotifyCollectionTrackMetadata> {
    let entity = root.props.page_props.state.data.entity;
    let track_index = resolve_requested_track_index(raw_track, entity.track_list.len())
        .ok_or_else(|| anyhow!("missing tracks"))?;

    let artwork_url = entity
        .visual_identity
        .image
        .iter()
        .max_by_key(|image| image.max_width)
        .map(|image| image.url.trim().to_string())
        .filter(|url| !url.is_empty())
        .ok_or_else(|| anyhow!("missing artwork"))?;

    let content_fingerprint = collection_content_fingerprint(&entity, &artwork_url);

    let track_names = entity
        .track_list
        .iter()
        .map(|track| track.title.trim().to_string())
        .collect::<Vec<_>>();

    if track_names.iter().all(|name| name.is_empty()) {
        return Err(anyhow!("missing track names"));
    }

    let track = entity
        .track_list
        .get(track_index)
        .ok_or_else(|| anyhow!("missing selected track"))?;

    let track_id = track
        .uri
        .rsplit_once(':')
        .map(|(_, tail)| tail)
        .ok_or_else(|| anyhow!("missing track id"))?;

    let song_name = track.title.trim().to_string();
    if song_name.is_empty() {
        return Err(anyhow!("missing song title"));
    }

    let artist_names = parse_artist_names(&track.subtitle)
        .into_iter()
        .map(|artist| artist.trim().to_string())
        .filter(|artist| !artist.is_empty())
        .collect::<Vec<_>>();
    if artist_names.is_empty() {
        return Err(anyhow!("missing artist names"));
    }

    let preview_url = track.audio_preview.url.trim().to_string();
    if preview_url.is_empty() {
        return Err(anyhow!("missing preview url"));
    }

    let track_metadata = SpotifyPreviewMetadata {
        media_id: track_id.to_string(),
        video_kind: VideoKind::Track,
        song_name,
        artist_names,
        preview_audio_url: preview_url,
        preview_video: None,
        album_art_url: artwork_url.clone(),
        artist_image_url: None,
        duration_ms: track.duration,
        release_date: None,
    };

    Ok(SpotifyCollectionTrackMetadata {
        collection_name: entity.title.trim().to_string(),
        collection_creator: entity.subtitle.trim().to_string(),
        artwork_url,
        selected_track_index: track_index,
        track_names,
        track: track_metadata,
        content_fingerprint,
    })
}

const COLLECTION_FINGERPRINT_LEN: usize = 16;

fn collection_content_fingerprint(entity: &CollectionEntity, artwork_url: &str) -> String {
    const FIELD_SEPARATOR: &[u8] = &[0x1f];

    fn feed(hasher: &mut Sha256, value: &str) {
        hasher.update(value.trim().as_bytes());
        hasher.update(FIELD_SEPARATOR);
    }

    let mut hasher = Sha256::new();
    feed(&mut hasher, &entity.title);
    feed(&mut hasher, &entity.subtitle);
    feed(&mut hasher, artwork_url);
    feed(&mut hasher, &entity.track_list.len().to_string());
    for track in &entity.track_list {
        feed(&mut hasher, &track.uri);
        feed(&mut hasher, &track.title);
        feed(&mut hasher, &track.subtitle);
        feed(
            &mut hasher,
            &track.duration.map(|d| d.to_string()).unwrap_or_default(),
        );
        feed(&mut hasher, &track.audio_preview.url);
    }

    let mut digest = hex::encode(hasher.finalize());
    digest.truncate(COLLECTION_FINGERPRINT_LEN);
    digest
}

fn resolve_requested_track_index(raw_track: Option<&str>, track_count: usize) -> Option<usize> {
    if track_count == 0 {
        return None;
    }

    let requested_index = raw_track
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .map(|value| value.saturating_sub(1))
        .unwrap_or(0);

    Some(requested_index.min(track_count - 1))
}

fn normalize_track_metadata(track_id: &str, root: TrackRoot) -> Result<SpotifyPreviewMetadata> {
    let entity = root.props.page_props.state.data.entity;
    let song_name = entity.title.trim().to_string();
    if song_name.is_empty() {
        return Err(anyhow!("missing song title"));
    }

    let artist_names = entity
        .artists
        .into_iter()
        .map(|artist| artist.name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    if artist_names.is_empty() {
        return Err(anyhow!("missing artist names"));
    }

    let preview_url = entity.audio_preview.url.trim().to_string();
    if preview_url.is_empty() {
        return Err(anyhow!("missing preview url"));
    }

    let album_art_url = entity
        .visual_identity
        .image
        .iter()
        .max_by_key(|image| image.max_width)
        .map(|image| image.url.trim().to_string())
        .filter(|url| !url.is_empty())
        .ok_or(anyhow!("missing album art"))?;

    let preview_video = if let Some(video_url) = entity.video_preview
        && let Some(video_thumbs) = entity.video_thumbnail_image
        && let Some(thumbnail_url) = video_thumbs.iter().max_by_key(|item| item.max_width)
    {
        Some(SpotifyMusicVideoPreview {
            video_url: video_url.url,
            thumbnail_url: thumbnail_url.url.clone(),
        })
    } else {
        None
    };

    Ok(SpotifyPreviewMetadata {
        media_id: track_id.to_string(),
        video_kind: VideoKind::Track,
        song_name,
        artist_names,
        preview_audio_url: preview_url,
        preview_video,
        album_art_url,
        artist_image_url: None,
        duration_ms: Some(entity.duration),
        release_date: Some(entity.release_date.iso_string),
    })
}

fn normalize_episode_metadata(
    episode_id: &str,
    root: EpisodeRoot,
) -> Result<SpotifyPreviewMetadata> {
    let entity = root.props.page_props.state.data.entity;

    let title = entity.title.trim().to_string();
    if title.is_empty() {
        return Err(anyhow!("missing episode title"));
    }

    let show_name = entity.subtitle.trim().to_string();
    if show_name.is_empty() {
        return Err(anyhow!("missing show name"));
    }

    let preview_url = entity.audio_preview.url.trim().to_string();
    if preview_url.is_empty() {
        return Err(anyhow!("missing preview url"));
    }

    let album_art_url = entity
        .visual_identity
        .image
        .iter()
        .max_by_key(|image| image.max_width)
        .map(|image| image.url.trim().to_string())
        .filter(|url| !url.is_empty())
        .ok_or(anyhow!("missing cover art"))?;

    let normalized_id =
        parse_media_id_from_uri(&entity.uri).unwrap_or_else(|| episode_id.to_string());

    Ok(SpotifyPreviewMetadata {
        media_id: normalized_id,
        video_kind: VideoKind::Episode,
        song_name: title,
        artist_names: vec![show_name],
        preview_audio_url: preview_url,
        album_art_url,
        artist_image_url: None,
        preview_video: None,
        duration_ms: entity.duration,
        release_date: entity.release_date.map(|date| date.iso_string),
    })
}

fn parse_media_id_from_uri(uri: &str) -> Option<String> {
    uri.rsplit_once(':')
        .map(|(_, tail)| tail.trim().to_string())
        .filter(|id| !id.is_empty())
}

fn parse_music_video_track_id(response: &serde_json::Value) -> Option<String> {
    let video = response
        .pointer("/data/trackUnion/associationsV3/videoAssociations/items/0/trackVideo/data")?;
    (video.get("mediaType")?.as_str()? == "VIDEO")
        .then(|| parse_media_id_from_uri(video.get("uri")?.as_str()?))?
}

fn parse_artist_names(subtitle: &str) -> Vec<String> {
    subtitle
        .replace('\u{a0}', " ")
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn extract_spotify_next_data_json(html_content: &str) -> Result<String> {
    let document = Html::parse_document(html_content);
    let selector = Selector::parse("#__NEXT_DATA__").map_err(|_| anyhow!("selector"))?;
    let element = document
        .select(&selector)
        .next()
        .ok_or(anyhow!("failed to find __NEXT_DATA__"))?;
    Ok(element.text().collect::<Vec<_>>().concat())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackRoot {
    props: TrackProps,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackProps {
    page_props: TrackPageProps,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackPageProps {
    state: TrackState,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackState {
    data: TrackData,
    settings: Option<TrackSettings>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackSettings {
    session: TrackSession,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackSession {
    access_token: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackData {
    entity: TrackEntity,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackEntity {
    title: String,
    artists: Vec<TrackArtist>,
    audio_preview: TrackAudioPreview,
    visual_identity: TrackVisualIdentity,
    video_preview: Option<TrackVideoPreview>,
    video_thumbnail_image: Option<Vec<TrackVideoThumbnailImage>>,
    duration: i64,
    release_date: TrackReleaseDate,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackReleaseDate {
    iso_string: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackVideoPreview {
    url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackVideoThumbnailImage {
    url: String,
    max_width: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackArtist {
    name: String,
    uri: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackAudioPreview {
    url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackVisualIdentity {
    image: Vec<TrackImage>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackImage {
    url: String,
    max_width: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CollectionRoot {
    props: CollectionProps,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CollectionProps {
    page_props: CollectionPageProps,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CollectionPageProps {
    state: CollectionState,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CollectionState {
    data: CollectionData,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CollectionData {
    entity: CollectionEntity,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CollectionEntity {
    title: String,
    subtitle: String,
    related_entity_uri: Option<String>,
    track_list: Vec<CollectionTrack>,
    visual_identity: TrackVisualIdentity,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CollectionTrack {
    uri: String,
    title: String,
    subtitle: String,
    audio_preview: TrackAudioPreview,
    duration: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EpisodeRoot {
    props: EpisodeProps,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EpisodeProps {
    page_props: EpisodePageProps,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EpisodePageProps {
    state: EpisodeState,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EpisodeState {
    data: EpisodeData,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EpisodeData {
    entity: EpisodeEntity,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EpisodeEntity {
    uri: String,
    title: String,
    subtitle: String,
    audio_preview: TrackAudioPreview,
    visual_identity: TrackVisualIdentity,
    duration: Option<i64>,
    release_date: Option<TrackReleaseDate>,
}

#[cfg(test)]
mod tests {
    use super::{
        CollectionRoot, SpotifyMetadataClient, normalize_collection_metadata, parse_artist_names,
        parse_music_video_track_id, resolve_requested_track_index,
    };
    use crate::embeds::cache_manager::MetadataCache;
    use bytes::Bytes;
    use serde_json::json;
    use std::time::Duration;

    #[tokio::test]
    async fn embed_lookup_uses_cached_json() {
        let cache = MetadataCache::new(1_024);
        cache
            .insert(
                "embed:https://invalid.example/embed".to_string(),
                Bytes::from_static(br#"{"cached":true}"#),
            )
            .await;
        let client = SpotifyMetadataClient::new(cache, Duration::from_secs(60));

        let value: serde_json::Value = client
            .fetch_spotify_embed_json("https://invalid.example/embed".to_string())
            .await
            .unwrap();

        assert_eq!(value, json!({"cached": true}));
    }

    #[tokio::test]
    async fn artist_lookup_uses_largest_cached_image() {
        let cache = MetadataCache::new(1_024);
        cache
            .insert(
                "embed:https://open.spotify.com/embed/artist/artist-id".to_string(),
                Bytes::from_static(
                    br#"{"props":{"pageProps":{"state":{"data":{"entity":{"visualIdentity":{"image":[{"url":"https://image/small.jpg","maxWidth":64},{"url":"https://image/large.jpg","maxWidth":640}]}}}}}}}"#,
                ),
            )
            .await;
        let client = SpotifyMetadataClient::new(cache, Duration::from_secs(60));

        let image_url = client.fetch_artist_image("artist-id").await.unwrap();

        assert_eq!(image_url, "https://image/large.jpg");
    }

    #[tokio::test]
    async fn partner_lookup_caches_tracks_without_music_videos() {
        let cache = MetadataCache::new(1_024);
        cache
            .insert(
                "partner:track-id".to_string(),
                Bytes::from_static(br#"{"data":{"trackUnion":{"associationsV3":null}}}"#),
            )
            .await;
        let client = SpotifyMetadataClient::new(cache, Duration::from_secs(60));

        let preview = client
            .fetch_music_video_preview("track-id", "invalid-token")
            .await
            .unwrap();

        assert!(preview.is_none());
    }

    #[test]
    fn resolve_requested_track_index_defaults_to_first_track() {
        assert_eq!(resolve_requested_track_index(None, 4), Some(0));
        assert_eq!(resolve_requested_track_index(Some(""), 4), Some(0));
        assert_eq!(resolve_requested_track_index(Some("0"), 4), Some(0));
    }

    #[test]
    fn resolve_requested_track_index_clamps_to_valid_bounds() {
        assert_eq!(resolve_requested_track_index(Some("2"), 4), Some(1));
        assert_eq!(resolve_requested_track_index(Some("99"), 4), Some(3));
    }

    #[test]
    fn resolve_requested_track_index_returns_none_when_no_tracks() {
        assert_eq!(resolve_requested_track_index(Some("1"), 0), None);
    }

    #[test]
    fn parse_artist_names_splits_comma_separated_subtitle() {
        let artists = parse_artist_names("Pitbull,\u{a0}Christina Aguilera");
        assert_eq!(artists, vec!["Pitbull", "Christina Aguilera"]);
    }

    #[test]
    fn parses_music_video_track_id_from_partner_response() {
        let response = json!({
            "data": {
                "trackUnion": {
                    "associationsV3": {
                        "videoAssociations": {
                            "items": [{
                                "trackVideo": {
                                    "data": {
                                        "uri": "spotify:track:5ms3smVOxBTyV6gULlXu1y",
                                        "mediaType": "VIDEO"
                                    }
                                }
                            }]
                        }
                    }
                }
            }
        });

        assert_eq!(
            parse_music_video_track_id(&response).as_deref(),
            Some("5ms3smVOxBTyV6gULlXu1y")
        );
    }

    #[test]
    fn playlist_metadata_uses_selected_track_audio_preview() {
        let root: CollectionRoot = serde_json::from_value(json!({
            "props": {
                "pageProps": {
                    "state": {
                        "data": {
                            "entity": {
                                "title": "Public playlist",
                                "subtitle": "Playlist curator",
                                "trackList": [
                                    {
                                        "uri": "spotify:track:first",
                                        "title": "First song",
                                        "subtitle": "First artist",
                                        "audioPreview": { "url": "https://preview/first.mp3" }
                                    },
                                    {
                                        "uri": "spotify:track:second",
                                        "title": "Second song",
                                        "subtitle": "Second artist",
                                        "audioPreview": { "url": "https://preview/second.mp3" }
                                    }
                                ],
                                "visualIdentity": {
                                    "image": [
                                        { "url": "https://image/small.jpg", "maxWidth": 64 },
                                        { "url": "https://image/large.jpg", "maxWidth": 640 }
                                    ]
                                }
                            }
                        }
                    }
                }
            }
        }))
        .unwrap();

        let metadata = normalize_collection_metadata(Some("2"), root).unwrap();

        assert_eq!(
            metadata.track.preview_audio_url,
            "https://preview/second.mp3"
        );
        assert_eq!(metadata.track.media_id, "second");
        assert_eq!(metadata.track.video_kind, crate::embeds::VideoKind::Track);
        assert_eq!(metadata.artwork_url, "https://image/large.jpg");
        assert_eq!(metadata.collection_name, "Public playlist");
        assert_eq!(metadata.collection_creator, "Playlist curator");
        assert!(!metadata.track_count_is_capped());
        assert_eq!(metadata.content_fingerprint.len(), 16);
    }

    fn playlist_root(title: &str, tracks: &[(&str, &str)]) -> CollectionRoot {
        let track_list = tracks
            .iter()
            .map(|(uri, name)| {
                json!({
                    "uri": uri,
                    "title": name,
                    "subtitle": "Artist",
                    "audioPreview": { "url": format!("https://preview/{name}.mp3") }
                })
            })
            .collect::<Vec<_>>();
        serde_json::from_value(json!({
            "props": {
                "pageProps": {
                    "state": {
                        "data": {
                            "entity": {
                                "title": title,
                                "subtitle": "Curator",
                                "trackList": track_list,
                                "visualIdentity": {
                                    "image": [{ "url": "https://image/cover.jpg", "maxWidth": 640 }]
                                }
                            }
                        }
                    }
                }
            }
        }))
        .unwrap()
    }

    fn fingerprint(root: CollectionRoot, track: Option<&str>) -> String {
        normalize_collection_metadata(track, root)
            .unwrap()
            .content_fingerprint
    }

    #[test]
    fn collection_fingerprint_is_stable_for_identical_content() {
        let tracks = [("spotify:track:a", "A"), ("spotify:track:b", "B")];
        let first = fingerprint(playlist_root("Mix", &tracks), None);
        let second = fingerprint(playlist_root("Mix", &tracks), None);
        assert_eq!(first, second);
    }

    #[test]
    fn collection_fingerprint_ignores_which_track_is_selected() {
        let tracks = [("spotify:track:a", "A"), ("spotify:track:b", "B")];
        let first = fingerprint(playlist_root("Mix", &tracks), Some("1"));
        let second = fingerprint(playlist_root("Mix", &tracks), Some("2"));
        assert_eq!(first, second);
    }

    #[test]
    fn collection_fingerprint_changes_when_playlist_changes() {
        let tracks = [("spotify:track:a", "A"), ("spotify:track:b", "B")];
        let base = fingerprint(playlist_root("Mix", &tracks), None);

        let renamed = fingerprint(playlist_root("Mix v2", &tracks), None);
        assert_ne!(base, renamed, "renaming the playlist must change it");

        let reordered = fingerprint(
            playlist_root("Mix", &[("spotify:track:b", "B"), ("spotify:track:a", "A")]),
            None,
        );
        assert_ne!(base, reordered, "reordering tracks must change it");

        let added = fingerprint(
            playlist_root(
                "Mix",
                &[
                    ("spotify:track:a", "A"),
                    ("spotify:track:b", "B"),
                    ("spotify:track:c", "C"),
                ],
            ),
            None,
        );
        assert_ne!(base, added, "adding a track must change it");

        let removed = fingerprint(playlist_root("Mix", &[("spotify:track:a", "A")]), None);
        assert_ne!(base, removed, "removing a track must change it");

        let retitled = fingerprint(
            playlist_root(
                "Mix",
                &[("spotify:track:a", "A!"), ("spotify:track:b", "B")],
            ),
            None,
        );
        assert_ne!(base, retitled, "a track title change must change it");
    }
}
