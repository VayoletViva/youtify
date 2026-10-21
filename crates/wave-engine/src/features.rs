//! Metadata heuristics, not measured acoustic features or actual BPM detection.
use core_types::TrackMetadata;
use serde::Deserialize;
use std::{collections::BTreeSet, time::Duration};
use tokio::{sync::Mutex, time::Instant};

use crate::Result;

pub type FeatureVector = [f32; 16];

/// Robust L2 normalization; zero stays zero and non-finite components are erased.
pub fn normalize(vector: &mut FeatureVector) {
    for value in vector.iter_mut() {
        if !value.is_finite() { *value = 0.0; }
    }
    let norm = vector.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>().sqrt();
    if norm > 0.0 {
        for value in vector { *value = (f64::from(*value) / norm) as f32; }
    }
}

pub fn dot_product(left: &FeatureVector, right: &FeatureVector) -> f32 {
    left.iter().zip(right).map(|(a, b)| a * b).sum::<f32>().clamp(-1.0, 1.0)
}

/// Stable across platforms and Rust versions, unlike a randomized map hasher.
fn tag_slot(tag: &str) -> usize {
    let hash = tag.bytes().fold(2_166_136_261_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(16_777_619)
    });
    (hash % 12) as usize
}

/// Slots 0..12: genre/mood hashing; 12: energy; 13: estimated BPM/240;
/// 14: duration/600s; 15: novelty from play count. Scalars are clamped before
/// L2 normalization. Unknown metadata gets neutral estimates, not invented tags.
pub fn vectorize(track: &TrackMetadata, tags: &[String], play_count: u64) -> FeatureVector {
    let title = track.title.to_lowercase();
    let words: BTreeSet<&str> = title.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty()).collect();
    let mut labels: BTreeSet<String> = tags.iter().take(64)
        .map(|tag| tag.trim().to_lowercase()).filter(|tag| !tag.is_empty()).collect();
    for label in ["phonk", "rock", "metal", "electronic", "techno", "house", "dance",
        "chill", "ambient", "lofi", "instrumental", "vocal", "jazz", "classical",
        "pop", "hiphop", "rap", "acoustic", "happy", "sad", "energetic", "relaxing"] {
        if words.contains(label) { labels.insert(label.to_owned()); }
    }
    if title.contains("hip-hop") || title.contains("hip hop") { labels.insert("hiphop".into()); }
    if title.contains("lo-fi") || title.contains("lo fi") { labels.insert("lofi".into()); }
    let has = |names: &[&str]| labels.iter().any(|label| names.contains(&label.as_str()));
    let (energy, bpm) = if has(&["metal", "phonk", "energetic"]) {
        (0.95, 150.0)
    } else if has(&["electronic", "techno", "house", "dance", "rock"]) {
        (0.8, 128.0)
    } else if has(&["ambient", "chill", "lofi", "relaxing", "classical"]) {
        (0.2, 75.0)
    } else if has(&["jazz", "acoustic", "sad"]) {
        (0.35, 95.0)
    } else { (0.5, 110.0) };

    let mut vector = [0.0; 16];
    // Give tags more influence than generic duration/tempo estimates.
    let tag_weight = 2.0 / (labels.len().max(1) as f32).sqrt();
    for label in &labels { vector[tag_slot(label)] += tag_weight; }
    vector[12] = energy;
    vector[13] = (bpm / 240.0_f32).clamp(0.0, 1.0);
    vector[14] = (track.duration_ms as f64 / 600_000.0).clamp(0.0, 1.0) as f32;
    vector[15] = 1.0 / (1.0 + play_count as f32).sqrt();
    normalize(&mut vector);
    vector
}

/// Optional MusicBrainz client. One instance should be shared by the app so the
/// one-request-per-second limit also applies to concurrent callers. Supply an
/// identifying User-Agent with your app version and contact URL/email.
pub struct TagClient {
    client: reqwest::Client,
    next_request: Mutex<Instant>,
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    recordings: Vec<Recording>,
}

#[derive(Deserialize)]
struct Recording {
    #[serde(default)]
    tags: Vec<Tag>,
}

#[derive(Deserialize)]
struct Tag { name: String }

impl TagClient {
    pub fn new(user_agent: &str) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder().user_agent(user_agent)
                .timeout(Duration::from_secs(5)).build()?,
            next_request: Mutex::new(Instant::now()),
        })
    }

    pub async fn tags(&self, track: &TrackMetadata) -> Result<Vec<String>> {
        let Some(artist) = track.artists.first() else { return Ok(Vec::new()); };
        if artist.trim().is_empty() || track.title.trim().is_empty() { return Ok(Vec::new()); }
        // Strip Lucene punctuation rather than letting metadata inject operators.
        let clean = |value: &str| value.chars().map(|c| {
            if c.is_alphanumeric() || c.is_whitespace() { c } else { ' ' }
        }).collect::<String>();
        let query = format!("recording:\"{}\" AND artist:\"{}\"", clean(&track.title), clean(artist));
        let mut next = self.next_request.lock().await;
        tokio::time::sleep_until(*next).await;
        *next = Instant::now() + Duration::from_secs(1);
        let response = self.client.get("https://musicbrainz.org/ws/2/recording/")
            .query(&[("query", query.as_str()), ("fmt", "json"), ("limit", "1")])
            .send().await?.error_for_status()?.json::<SearchResponse>().await?;
        Ok(response.recordings.into_iter().next().map(|recording| {
            recording.tags.into_iter().take(64).map(|tag| tag.name).collect()
        }).unwrap_or_default())
    }

    /// Offline, rate-limited, missing-tag and HTTP-error cases all have a local
    /// fallback. Persist the resulting vector once; ranking never fetches tags.
    pub async fn vectorize_or_fallback(&self, track: &TrackMetadata, play_count: u64) -> FeatureVector {
        let tags = self.tags(track).await.unwrap_or_default();
        vectorize(track, &tags, play_count)
    }
}