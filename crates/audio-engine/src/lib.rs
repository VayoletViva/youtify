//! Bounded, worker-decoded playback through one permanent output source.
pub mod decoder;
mod engine;
mod gapless;
pub mod hls;
pub mod streamer;

pub use engine::AudioEngine;
use core_types::{StreamInfo, TrackMetadata};
use std::time::Duration;

#[derive(Debug, Clone)]
pub enum PlayerCommand {
    Play(TrackMetadata, StreamInfo),
    Pause,
    Resume,
    Skip,
    Seek(Duration),
    SetVolume(f32),
    QueueNext(TrackMetadata, StreamInfo),
    SetCrossfade(Duration),
}

#[derive(Debug, Clone)]
pub enum PlayerEvent {
    TrackStarted { track: TrackMetadata },
    Progress { track_id: String, position_ms: u64, duration_ms: u64 },
    PreloadTriggered { track_id: String },
    /// At least one decoded PCM block is available (not a full-track download).
    PreloadReady { track_id: String },
    /// Next track supplied a sample without inserting silence at the boundary.
    GaplessTransition { from_track_id: String, to_track_id: String },
    /// Ratio is *listened samples / duration*, not the seek cursor. Replays
    /// after a backward seek count as listening; the result is clamped to 1.
    TrackEnded { track_id: String, completion_ratio: f32, skipped_early: bool },
    Error { track_id: Option<String>, message: String },
    QueueFinished,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("audio engine: {0}")]
    Message(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

pub(crate) type Result<T> = std::result::Result<T, EngineError>;