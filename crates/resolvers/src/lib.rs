//! Independent HTTP adapters. No playback, filesystem state, or external processes.
pub mod deezer;
pub mod soundcloud;
pub mod spotify;
pub mod youtube;

mod http;

use core_types::{ResolverError, TrackMetadata};
use serde_json::Value;

pub const DURATION_TOLERANCE_MS: u64 = 3_500;

pub fn duration_matches(expected_ms: u64, actual_ms: u64) -> bool {
    expected_ms > 0 && actual_ms > 0 && expected_ms.abs_diff(actual_ms) <= DURATION_TOLERANCE_MS
}

fn queries(track: &TrackMetadata) -> Result<Vec<String>, ResolverError> {
    if track.duration_ms == 0 || track.title.trim().is_empty() {
        return Err(ResolverError::InvalidInput("title and positive duration_ms are required".into()));
    }
    let mut queries = Vec::new();
    if let Some(isrc) = track.isrc.as_deref().filter(|s| !s.trim().is_empty()) {
        queries.push(isrc.to_owned());
    }
    queries.push(if track.artists.is_empty() {
        track.title.clone()
    } else {
        format!("{} - {}", track.artists.join(", "), track.title)
    });
    Ok(queries)
}

fn parse_error(service: &'static str, message: impl Into<String>) -> ResolverError {
    ResolverError::Parse { service, message: message.into() }
}

fn string(value: &Value, pointer: &str) -> Option<String> {
    value.pointer(pointer).and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty()).map(str::to_owned)
}

fn required_string(value: &Value, pointer: &str, service: &'static str) -> Result<String, ResolverError> {
    string(value, pointer).ok_or_else(|| parse_error(service, format!("missing {pointer}")))
}

fn expires_at(url: &str) -> Option<u64> {
    url::Url::parse(url).ok()?.query_pairs()
        .find(|(key, _)| key == "expire" || key == "expires" || key == "Expires")?
        .1.parse().ok()
}
