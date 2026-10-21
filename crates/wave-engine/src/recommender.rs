use std::{cmp::{Ordering, Reverse}, collections::{BinaryHeap, HashSet, VecDeque}, time::Duration};
use core_types::{MetadataProvider, TrackMetadata};
use rand::Rng;

use crate::{features::{dot_product, normalize, vectorize, FeatureVector}, Result, WaveDb, WaveError};

const MAX_BATCH: usize = 100;

#[derive(Debug, Clone)]
pub struct WaveConfig {
    /// Fraction of controlled random/novelty influence, not a guaranteed quota.
    pub exploration_rate: f32,
    pub half_life_seconds: f64,
    pub recent_penalty: f32,
}

impl Default for WaveConfig {
    fn default() -> Self {
        Self { exploration_rate: 0.15, half_life_seconds: 7.0 * 86_400.0, recent_penalty: 1.0 }
    }
}

impl WaveConfig {
    fn validate(&self) -> Result<()> {
        if !self.exploration_rate.is_finite() || !(0.0..=1.0).contains(&self.exploration_rate)
            || !self.half_life_seconds.is_finite() || self.half_life_seconds <= 0.0
            || !self.recent_penalty.is_finite() || !(0.0..=10.0).contains(&self.recent_penalty) {
            return Err(WaveError::InvalidData("invalid recommendation configuration".into()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct UserTasteProfile {
    pub vector: FeatureVector,
    pub interaction_count: usize,
}

impl UserTasteProfile {
    pub fn from_recent(db: &WaveDb, now: i64, config: &WaveConfig) -> Result<Self> {
        config.validate()?;
        if now < 0 { return Err(WaveError::InvalidData("negative current timestamp".into())); }
        let events = db.recent_interactions()?;
        let mut sum = [0.0_f64; 16];
        // Factoring out the newest meaningful event's common decay avoids
        // underflow after a long offline interval without changing direction.
        let weight = |event: &crate::Interaction| -> f64 {
            if event.liked { 2.0 }
            else if event.skipped_early || event.completion_ratio < 0.15 { -1.5 }
            else if event.completion_ratio > 0.8 { 1.0 }
            else { 0.0 }
        };
        let base_age = events.iter().filter(|(event, _)| weight(event) != 0.0)
            .map(|(event, _)| now.saturating_sub(event.timestamp).max(0)).min().unwrap_or(0);
        for (event, vector) in &events {
            let age = now.saturating_sub(event.timestamp).max(0).saturating_sub(base_age);
            let contribution = weight(event) * (-std::f64::consts::LN_2 * age as f64
                / config.half_life_seconds).exp();
            for (total, value) in sum.iter_mut().zip(vector) { *total += contribution * f64::from(*value); }
        }
        let mut vector = sum.map(|value| value as f32);
        normalize(&mut vector);
        Ok(Self { vector, interaction_count: events.len() })
    }
}

#[derive(Debug, Clone)]
pub struct ScoredTrack {
    pub track: TrackMetadata,
    /// Ranking score includes exploration and the recent-play penalty.
    pub score: f32,
    /// Pure cosine similarity, kept separately for inspection.
    pub similarity: f32,
}

struct Candidate { id: String, score: f32, similarity: f32 }

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool { self.cmp(other) == Ordering::Equal }
}
impl Eq for Candidate {}
impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}
impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score.total_cmp(&other.score).then_with(|| self.id.cmp(&other.id))
    }
}

/// O(N * (16 + log(batch_size))) time, O(batch_size + 50) working memory.
/// Pass queued/current IDs in `excluded` to prevent duplicate enqueueing.
pub fn next_wave_batch(
    db: &WaveDb,
    count: usize,
    now: i64,
    config: &WaveConfig,
    excluded: &HashSet<String>,
) -> Result<Vec<ScoredTrack>> {
    next_wave_batch_with_rng(db, count, now, config, excluded, &mut rand::thread_rng())
}

/// Inject a seeded RNG for reproducible tests or debugging.
pub fn next_wave_batch_with_rng(
    db: &WaveDb,
    count: usize,
    now: i64,
    config: &WaveConfig,
    excluded: &HashSet<String>,
    rng: &mut impl Rng,
) -> Result<Vec<ScoredTrack>> {
    config.validate()?;
    if count > MAX_BATCH { return Err(WaveError::InvalidData("batch size must be <= 100".into())); }
    if count == 0 { return Ok(Vec::new()); }
    let profile = UserTasteProfile::from_recent(db, now, config)?;
    let mut best: BinaryHeap<Reverse<Candidate>> = BinaryHeap::with_capacity(count);
    db.visit_candidates(|id, vector, plays, last_played| {
        if excluded.contains(&id) { return; }
        let similarity = dot_product(&profile.vector, &vector);
        let novelty = 1.0 / (1.0 + plays.max(0) as f32).sqrt();
        let exploration = if config.exploration_rate > 0.0 {
            config.exploration_rate * (0.5 * novelty + rng.gen_range(-0.5..=0.5))
        } else { 0.0 };
        let recent = plays > 0 && now.saturating_sub(last_played) < 2 * 60 * 60;
        let score = similarity + exploration - if recent { config.recent_penalty } else { 0.0 };
        let candidate = Candidate { id, score, similarity };
        if best.len() < count { best.push(Reverse(candidate)); }
        else if best.peek().is_some_and(|worst| candidate > worst.0) {
            best.pop();
            best.push(Reverse(candidate));
        }
    })?;
    let mut candidates: Vec<_> = best.into_iter().map(|entry| entry.0).collect();
    candidates.sort_unstable_by(|left, right| right.cmp(left));
    candidates.into_iter().map(|candidate| Ok(ScoredTrack {
        track: db.track(&candidate.id)?, score: candidate.score, similarity: candidate.similarity,
    })).collect()
}

/// Bounded metadata queue. Popped tracks remain reserved until `release` is
/// called after TrackEnded (or stream-resolution failure). Audio integration
/// resolves the returned metadata and sends PlayerCommand::QueueNext itself.
pub struct WaveQueue {
    pending: VecDeque<ScoredTrack>,
    reserved: HashSet<String>,
    target_len: usize,
}

impl WaveQueue {
    pub fn new(target_len: usize) -> Result<Self> {
        if target_len == 0 || target_len > MAX_BATCH {
            return Err(WaveError::InvalidData("queue length must be 1..=100".into()));
        }
        Ok(Self { pending: VecDeque::new(), reserved: HashSet::new(), target_len })
    }

    pub fn refill(&mut self, db: &WaveDb, now: i64, config: &WaveConfig) -> Result<usize> {
        let needed = self.target_len.saturating_sub(self.reserved.len());
        let batch = next_wave_batch(db, needed, now, config, &self.reserved)?;
        let added = batch.len();
        for track in batch {
            self.reserved.insert(track.track.id.clone());
            self.pending.push_back(track);
        }
        Ok(added)
    }

    pub fn pop_next(&mut self) -> Option<ScoredTrack> { self.pending.pop_front() }

    pub fn release(&mut self, track_id: &str) {
        self.reserved.remove(track_id);
        self.pending.retain(|item| item.track.id != track_id);
    }

    /// Refill on demand, never from a periodic polling task.
    pub fn next_track(&mut self, db: &WaveDb, now: i64, config: &WaveConfig) -> Result<Option<ScoredTrack>> {
        self.refill(db, now, config)?;
        Ok(self.pop_next())
    }
}

#[derive(Debug, Default)]
pub struct DiscoveryReport {
    pub inserted: usize,
    pub errors: Vec<String>,
}

/// Optional bounded discovery through the existing MetadataProvider contract.
/// Caller supplies queries for adjacent artists/tags; that information is not
/// exposed by StreamResolver. No resolver implementation or network access is
/// coupled to local ranking. Existing IDs are skipped, preserving enriched vectors.
pub async fn discover_tracks(
    db: &WaveDb,
    provider: &impl MetadataProvider,
    queries: &[String],
) -> Result<DiscoveryReport> {
    let mut report = DiscoveryReport::default();
    for query in queries.iter().take(8) {
        match tokio::time::timeout(Duration::from_secs(10), provider.track(query)).await {
            Ok(Ok(track)) => {
                if !db.contains_track(&track.id)? {
                    let vector = vectorize(&track, &[], 0);
                    db.upsert_track(&track, vector)?;
                    report.inserted += 1;
                }
            }
            Ok(Err(error)) => report.errors.push(error.to_string()),
            Err(_) => report.errors.push(format!("discovery timed out: {query}")),
        }
    }
    Ok(report)
}