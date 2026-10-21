//! Service-independent metadata and stream resolution contracts.
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Use the same identity when probing and downloading signed media URLs.
pub const MEDIA_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/130.0.0.0 Safari/537.36";

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TrackMetadata {
    /// Provider-qualified ID, e.g. `spotify:track:...`.
    pub id: String,
    pub title: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub duration_ms: u64,
    pub cover_url: Option<String>,
    pub isrc: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    Opus,
    Aac,
    Mp3,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamSource {
    YouTube,
    SoundCloud,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StreamInfo {
    /// Direct media URL or HLS manifest URL, never a service landing page.
    pub stream_url: String,
    pub codec: AudioCodec,
    /// Bits per second, if reported by the provider.
    pub bitrate: Option<u32>,
    pub source: StreamSource,
    /// Unix timestamp in seconds, if reported by the provider.
    pub expires_at: Option<u64>,
}

#[derive(Debug, Error)]
pub enum ResolverError {
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("{service}: transport error: {message}")]
    Transport { service: &'static str, message: String },
    #[error("{service}: HTTP {status}")]
    Http { service: &'static str, status: u16 },
    #[error("{service}: HTTP 429 (rate limited); retry in {retry_after_secs} seconds")]
    RateLimited { service: &'static str, retry_after_secs: u64 },
    #[error("{service}: invalid response: {message}")]
    Parse { service: &'static str, message: String },
    #[error("{service}: unavailable: {message}")]
    Unavailable { service: &'static str, message: String },
    #[error("{service}: no match for {query}")]
    NotFound { service: &'static str, query: String },
    #[error("{service}: unsupported operation: {operation}")]
    Unsupported { service: &'static str, operation: &'static str },
}

#[allow(async_fn_in_trait)]
pub trait MetadataProvider {
    /// Resolve a public track URL, provider ID, or search query.
    async fn track(&self, input: &str) -> Result<TrackMetadata, ResolverError>;

    /// Return all playlist tracks; incomplete playlists must produce an error.
    async fn playlist(&self, input: &str) -> Result<Vec<TrackMetadata>, ResolverError>;
}

#[allow(async_fn_in_trait)]
pub trait StreamResolver {
    async fn resolve(&self, track: &TrackMetadata) -> Result<StreamInfo, ResolverError>;
}
