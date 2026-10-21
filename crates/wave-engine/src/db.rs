use std::{path::Path, time::Duration};
use core_types::TrackMetadata;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::{features::{normalize, FeatureVector}, Result, WaveError};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Interaction {
    pub track_id: String,
    pub completion_ratio: f32,
    pub skipped_early: bool,
    pub liked: bool,
    /// Unix seconds, not milliseconds.
    pub timestamp: i64,
}

pub struct WaveDb { connection: Connection }

impl WaveDb {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::initialize(Connection::open(path)?)
    }

    pub fn in_memory() -> Result<Self> { Self::initialize(Connection::open_in_memory()?) }

    fn initialize(connection: Connection) -> Result<Self> {
        connection.busy_timeout(Duration::from_secs(2))?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA cache_size = -1024;
             CREATE TABLE IF NOT EXISTS tracks (
                 id TEXT PRIMARY KEY NOT NULL,
                 metadata_json TEXT NOT NULL,
                 feature_vector BLOB NOT NULL CHECK(length(feature_vector) = 64),
                 play_count INTEGER NOT NULL DEFAULT 0,
                 skip_count INTEGER NOT NULL DEFAULT 0,
                 last_played_ts INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS interactions (
                 id INTEGER PRIMARY KEY,
                 track_id TEXT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
                 completion_ratio REAL NOT NULL CHECK(completion_ratio BETWEEN 0 AND 1),
                 skipped_early INTEGER NOT NULL CHECK(skipped_early IN (0, 1)),
                 liked INTEGER NOT NULL CHECK(liked IN (0, 1)),
                 timestamp INTEGER NOT NULL CHECK(timestamp >= 0)
             );
             CREATE INDEX IF NOT EXISTS interactions_recent
                 ON interactions(timestamp DESC, id DESC);
             CREATE INDEX IF NOT EXISTS interactions_track ON interactions(track_id);"
        )?;
        Ok(Self { connection })
    }

    /// Metadata/vector refresh never resets listening history.
    pub fn upsert_track(&self, track: &TrackMetadata, mut vector: FeatureVector) -> Result<()> {
        if track.id.trim().is_empty() || vector.iter().any(|v| !v.is_finite()) {
            return Err(WaveError::InvalidData("empty ID or non-finite vector".into()));
        }
        normalize(&mut vector);
        let blob: Vec<u8> = vector.iter().flat_map(|value| value.to_le_bytes()).collect();
        self.connection.execute(
            "INSERT INTO tracks(id, metadata_json, feature_vector) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET metadata_json=excluded.metadata_json,
                 feature_vector=excluded.feature_vector",
            params![track.id, serde_json::to_string(track)?, blob],
        )?;
        Ok(())
    }

    pub fn contains_track(&self, id: &str) -> Result<bool> {
        Ok(self.connection.query_row("SELECT EXISTS(SELECT 1 FROM tracks WHERE id=?1)",
            [id], |row| row.get(0))?)
    }

    pub fn track(&self, id: &str) -> Result<TrackMetadata> {
        let json: Option<String> = self.connection.query_row(
            "SELECT metadata_json FROM tracks WHERE id=?1", [id], |row| row.get(0),
        ).optional()?;
        Ok(serde_json::from_str(&json.ok_or_else(|| WaveError::UnknownTrack(id.into()))?)?)
    }

    /// Atomically log a TrackEnded-equivalent event and update counters.
    /// Explicit likes win over skips when building the taste vector; the actual
    /// skip flag is still retained here. Caller should submit each event once.
    pub fn record_interaction(&mut self, event: &Interaction) -> Result<()> {
        if !event.completion_ratio.is_finite() || !(0.0..=1.0).contains(&event.completion_ratio)
            || event.timestamp < 0 {
            return Err(WaveError::InvalidData("invalid completion ratio or timestamp".into()));
        }
        let transaction = self.connection.transaction()?;
        let skip = event.skipped_early || event.completion_ratio < 0.15;
        let changed = transaction.execute(
            "UPDATE tracks SET play_count=play_count+1, skip_count=skip_count+?2,
                last_played_ts=MAX(last_played_ts, ?3) WHERE id=?1",
            params![event.track_id, skip, event.timestamp],
        )?;
        if changed == 0 { return Err(WaveError::UnknownTrack(event.track_id.clone())); }
        transaction.execute(
            "INSERT INTO interactions(track_id, completion_ratio, skipped_early, liked, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![event.track_id, event.completion_ratio, event.skipped_early, event.liked, event.timestamp],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn recent_interactions(&self) -> Result<Vec<(Interaction, FeatureVector)>> {
        let mut statement = self.connection.prepare_cached(
            "SELECT i.track_id, i.completion_ratio, i.skipped_early, i.liked, i.timestamp,
                    t.feature_vector FROM interactions i JOIN tracks t ON t.id=i.track_id
             ORDER BY i.timestamp DESC, i.id DESC LIMIT 50",
        )?;
        let mut rows = statement.query([])?;
        let mut events = Vec::with_capacity(50);
        while let Some(row) = rows.next()? {
            let event = Interaction {
                track_id: row.get(0)?, completion_ratio: row.get(1)?,
                skipped_early: row.get(2)?, liked: row.get(3)?, timestamp: row.get(4)?,
            };
            let blob: Vec<u8> = row.get(5)?;
            events.push((event, decode_vector(&blob)?));
        }
        Ok(events)
    }

    /// Scan only IDs, 64-byte vectors and counters. Do not deserialize metadata
    /// or load the whole library into memory while scoring.
    pub(crate) fn visit_candidates(
        &self,
        mut visit: impl FnMut(String, FeatureVector, i64, i64),
    ) -> Result<()> {
        let mut statement = self.connection.prepare_cached(
            "SELECT id, feature_vector, play_count, last_played_ts FROM tracks ORDER BY id",
        )?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let blob: Vec<u8> = row.get(1)?;
            visit(row.get(0)?, decode_vector(&blob)?, row.get(2)?, row.get(3)?);
        }
        Ok(())
    }
}

fn decode_vector(blob: &[u8]) -> Result<FeatureVector> {
    if blob.len() != 64 { return Err(WaveError::InvalidData("vector must contain 64 bytes".into())); }
    let mut vector = [0.0; 16];
    for (value, bytes) in vector.iter_mut().zip(blob.chunks_exact(4)) {
        *value = f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    }
    if vector.iter().any(|v| !v.is_finite()) {
        return Err(WaveError::InvalidData("non-finite stored vector".into()));
    }
    Ok(vector)
}