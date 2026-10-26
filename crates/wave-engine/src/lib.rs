//! Local, on-demand recommendations. No background polling or audio decoding.
//!
//! SQLite work is synchronous: own the database on a worker thread (or use
//! `tokio::task::spawn_blocking`), never on the audio callback. Network enrichment
//! and discovery are optional, explicit operations, not part of ranking.
pub mod db;
pub mod features;
pub mod recommender;

pub use db::{Interaction, WaveDb};
pub use features::{FeatureVector, TagClient};
pub use recommender::{
    discover_tracks, DiscoveryReport, ScoredTrack, UserTasteProfile, WaveConfig, WaveQueue,
    next_wave_batch, next_wave_mix,
};

#[derive(Debug, thiserror::Error)]
pub enum WaveError {
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error("invalid wave data: {0}")]
    InvalidData(String),
    #[error("track not stored: {0}")]
    UnknownTrack(String),
}

pub type Result<T> = std::result::Result<T, WaveError>;

/// Unix seconds; pre-epoch system clocks are treated as zero.
pub fn unix_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0)
}