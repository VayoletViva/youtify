use std::{path::{Path, PathBuf}, time::Duration};
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
             CREATE INDEX IF NOT EXISTS interactions_track ON interactions(track_id);
             CREATE TABLE IF NOT EXISTS downloads (
                 track_id TEXT PRIMARY KEY NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
                 path TEXT NOT NULL,
                 bytes INTEGER NOT NULL CHECK(bytes >= 0),
                 created_ts INTEGER NOT NULL
             );"
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

    /// Recently liked or mostly-completed tracks, newest first, distinct; seeds for discovery.
    /// Topped up with the newest stored tracks never skipped early, so a fresh
    /// library still yields seeds.
    pub fn seed_tracks(&self, limit: usize) -> Result<Vec<TrackMetadata>> {
        if limit == 0 { return Ok(Vec::new()); }
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut seeds = Vec::new();
        let mut statement = self.connection.prepare_cached(
            "SELECT t.metadata_json FROM tracks t JOIN (
                 SELECT track_id, MAX(timestamp) AS ts, MAX(id) AS last_id FROM interactions
                 WHERE liked=1 OR (completion_ratio >= 0.6 AND skipped_early=0)
                 GROUP BY track_id
             ) s ON s.track_id=t.id
             ORDER BY s.ts DESC, s.last_id DESC LIMIT ?1",
        )?;
        let mut rows = statement.query([limit])?;
        while let Some(row) = rows.next()? { seeds.push(serde_json::from_str(&row.get::<_, String>(0)?)?); }
        let missing = limit - seeds.len() as i64;
        if missing > 0 {
            // A shortfall means every qualifying track is already in `seeds`, so
            // excluding qualifying tracks in SQL keeps the top-up distinct.
            let mut statement = self.connection.prepare_cached(
                "SELECT t.metadata_json FROM tracks t
                 WHERE NOT EXISTS(SELECT 1 FROM interactions i WHERE i.track_id=t.id
                     AND (i.skipped_early=1 OR i.liked=1 OR i.completion_ratio >= 0.6))
                 ORDER BY t.rowid DESC LIMIT ?1",
            )?;
            let mut rows = statement.query([missing])?;
            while let Some(row) = rows.next()? { seeds.push(serde_json::from_str(&row.get::<_, String>(0)?)?); }
        }
        Ok(seeds)
    }

    /// Stored tracks never played, excluding `excluded`; tells when discovery is needed.
    /// O(|excluded|) point lookups instead of streaming the library.
    pub fn unplayed_count(&self, excluded: &std::collections::HashSet<String>) -> Result<usize> {
        let total: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM tracks WHERE play_count=0", [], |row| row.get(0))?;
        let mut statement = self.connection.prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM tracks WHERE id=?1 AND play_count=0)")?;
        let mut hidden = 0_i64;
        for id in excluded {
            if statement.query_row([id], |row| row.get::<_, bool>(0))? { hidden += 1; }
        }
        Ok(usize::try_from(total - hidden).unwrap_or(0))
    }

    /// Remember the local file of a stored track (replaces an older path).
    /// REPLACE assigns a new rowid, so a re-download sorts as newest.
    pub fn set_download(&self, track_id: &str, path: &Path, bytes: u64) -> Result<()> {
        let text = path.to_str()
            .ok_or_else(|| WaveError::InvalidData("download path is not UTF-8".into()))?;
        if text.is_empty() { return Err(WaveError::InvalidData("empty download path".into())); }
        let bytes = i64::try_from(bytes)
            .map_err(|_| WaveError::InvalidData("download size out of range".into()))?;
        let changed = self.connection.execute(
            "INSERT OR REPLACE INTO downloads(track_id, path, bytes, created_ts)
             SELECT ?1, ?2, ?3, ?4 WHERE EXISTS(SELECT 1 FROM tracks WHERE id=?1)",
            params![track_id, text, bytes, crate::unix_timestamp()],
        )?;
        if changed == 0 { return Err(WaveError::UnknownTrack(track_id.into())); }
        Ok(())
    }

    pub fn download_path(&self, track_id: &str) -> Result<Option<PathBuf>> {
        Ok(self.connection.query_row(
            "SELECT path FROM downloads WHERE track_id=?1", [track_id],
            |row| row.get::<_, String>(0),
        ).optional()?.map(PathBuf::from))
    }

    /// Forget a download and return its former path (the caller deletes the file).
    pub fn remove_download(&self, track_id: &str) -> Result<Option<PathBuf>> {
        // RETURNING applies the delete on the first step, so one query is atomic.
        Ok(self.connection.query_row(
            "DELETE FROM downloads WHERE track_id=?1 RETURNING path", [track_id],
            |row| row.get::<_, String>(0),
        ).optional()?.map(PathBuf::from))
    }

    /// All downloads, newest first: metadata, path and size in bytes.
    pub fn downloads(&self) -> Result<Vec<(TrackMetadata, PathBuf, u64)>> {
        let mut statement = self.connection.prepare_cached(
            "SELECT t.metadata_json, d.path, d.bytes FROM downloads d JOIN tracks t ON t.id=d.track_id
             ORDER BY d.created_ts DESC, d.rowid DESC",
        )?;
        let mut rows = statement.query([])?;
        let mut downloads = Vec::new();
        while let Some(row) = rows.next()? {
            let bytes: i64 = row.get(2)?;
            downloads.push((serde_json::from_str(&row.get::<_, String>(0)?)?,
                PathBuf::from(row.get::<_, String>(1)?), bytes.max(0) as u64));
        }
        Ok(downloads)
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
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn track(id: &str) -> TrackMetadata {
        TrackMetadata { id: id.into(), title: id.into(), artists: vec!["A".into()], album: None,
            duration_ms: 180_000, cover_url: None, isrc: None }
    }

    fn library(ids: &[&str]) -> WaveDb {
        let db = WaveDb::in_memory().unwrap();
        for id in ids { db.upsert_track(&track(id), [1.0; 16]).unwrap(); }
        db
    }

    fn event(id: &str, completion_ratio: f32, skipped_early: bool, liked: bool, timestamp: i64) -> Interaction {
        Interaction { track_id: id.into(), completion_ratio, skipped_early, liked, timestamp }
    }

    fn ids(tracks: &[TrackMetadata]) -> Vec<&str> { tracks.iter().map(|t| t.id.as_str()).collect() }

    #[test]
    fn download_crud_and_ordering() {
        let db = library(&["a", "b"]);
        assert_eq!(db.download_path("a").unwrap(), None);
        db.set_download("a", Path::new("/music/a.opus"), 10).unwrap();
        db.set_download("b", Path::new("/music/b.m4a"), 20).unwrap();
        assert_eq!(db.download_path("a").unwrap(), Some(PathBuf::from("/music/a.opus")));
        let listed: Vec<_> = db.downloads().unwrap().into_iter()
            .map(|(meta, path, bytes)| (meta.id, path, bytes)).collect();
        assert_eq!(listed, vec![("b".into(), PathBuf::from("/music/b.m4a"), 20),
            ("a".into(), PathBuf::from("/music/a.opus"), 10)]);
        // Re-download replaces the path and becomes the newest entry.
        db.set_download("a", Path::new("/music/a2.opus"), 11).unwrap();
        let first = &db.downloads().unwrap()[0];
        assert_eq!((first.0.id.as_str(), first.1.as_path(), first.2), ("a", Path::new("/music/a2.opus"), 11));
        assert_eq!(db.downloads().unwrap().len(), 2);
        assert_eq!(db.remove_download("a").unwrap(), Some(PathBuf::from("/music/a2.opus")));
        assert_eq!(db.remove_download("a").unwrap(), None);
        assert_eq!(db.download_path("a").unwrap(), None);
        assert_eq!(db.downloads().unwrap().len(), 1);
    }

    #[test]
    fn download_requires_stored_track_and_valid_input() {
        let db = library(&["a"]);
        assert!(matches!(db.set_download("missing", Path::new("/x.opus"), 1),
            Err(WaveError::UnknownTrack(id)) if id == "missing"));
        assert!(matches!(db.set_download("a", Path::new(""), 1), Err(WaveError::InvalidData(_))));
        assert!(matches!(db.set_download("a", Path::new("/x"), u64::MAX), Err(WaveError::InvalidData(_))));
        assert!(db.downloads().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn download_rejects_non_utf8_path() {
        use std::os::unix::ffi::OsStrExt;
        let db = library(&["a"]);
        let path = Path::new(std::ffi::OsStr::from_bytes(b"/music/\xff.opus"));
        assert!(matches!(db.set_download("a", path, 1), Err(WaveError::InvalidData(_))));
    }

    #[cfg(windows)]
    #[test]
    fn download_rejects_non_utf8_path() {
        use std::os::windows::ffi::OsStringExt;
        let db = library(&["a"]);
        let path = PathBuf::from(std::ffi::OsString::from_wide(&[0x43, 0x3a, 0x5c, 0xd800]));
        assert!(matches!(db.set_download("a", &path, 1), Err(WaveError::InvalidData(_))));
    }

    #[test]
    fn download_cascades_on_track_delete() {
        let db = library(&["a"]);
        db.set_download("a", Path::new("/music/a.opus"), 1).unwrap();
        db.connection.execute("DELETE FROM tracks WHERE id='a'", []).unwrap();
        assert!(db.downloads().unwrap().is_empty());
    }

    #[test]
    fn initialize_upgrades_database_without_downloads_table() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(
            "CREATE TABLE tracks (id TEXT PRIMARY KEY NOT NULL, metadata_json TEXT NOT NULL,
                 feature_vector BLOB NOT NULL CHECK(length(feature_vector) = 64),
                 play_count INTEGER NOT NULL DEFAULT 0, skip_count INTEGER NOT NULL DEFAULT 0,
                 last_played_ts INTEGER NOT NULL DEFAULT 0);").unwrap();
        let db = WaveDb::initialize(connection).unwrap();
        db.upsert_track(&track("a"), [1.0; 16]).unwrap();
        db.set_download("a", Path::new("/music/a.opus"), 5).unwrap();
        assert_eq!(db.downloads().unwrap().len(), 1);
    }

    #[test]
    fn seeds_order_by_latest_qualifying_event_then_top_up() {
        let mut db = library(&["a", "b", "c", "d", "e"]);
        db.record_interaction(&event("a", 0.2, false, true, 10)).unwrap();
        db.record_interaction(&event("b", 0.9, false, false, 30)).unwrap();
        db.record_interaction(&event("c", 0.9, true, false, 40)).unwrap();
        db.record_interaction(&event("d", 0.3, false, false, 50)).unwrap();
        // c qualifies by completion but was skipped early: never a seed, not even in the top-up.
        assert_eq!(ids(&db.seed_tracks(10).unwrap()), ["b", "a", "e", "d"]);
        assert_eq!(ids(&db.seed_tracks(1).unwrap()), ["b"]);
        assert_eq!(ids(&db.seed_tracks(3).unwrap()), ["b", "a", "e"]);
        assert!(db.seed_tracks(0).unwrap().is_empty());
        // A newer qualifying listen moves a to the front without duplicating it.
        db.record_interaction(&event("a", 0.6, false, false, 60)).unwrap();
        assert_eq!(ids(&db.seed_tracks(10).unwrap()), ["a", "b", "e", "d"]);
    }

    #[test]
    fn seeds_fall_back_to_newest_tracks_on_empty_history() {
        let db = library(&["a", "b", "c"]);
        assert_eq!(ids(&db.seed_tracks(2).unwrap()), ["c", "b"]);
        assert!(WaveDb::in_memory().unwrap().seed_tracks(5).unwrap().is_empty());
    }

    #[test]
    fn unplayed_count_honours_exclusions() {
        let mut db = library(&["a", "b", "c", "d"]);
        db.record_interaction(&event("a", 1.0, false, false, 10)).unwrap();
        assert_eq!(db.unplayed_count(&HashSet::new()).unwrap(), 3);
        // Excluding a played or unknown id must not reduce the count.
        let excluded: HashSet<String> = ["a", "b", "zzz"].map(String::from).into();
        assert_eq!(db.unplayed_count(&excluded).unwrap(), 2);
    }
}
