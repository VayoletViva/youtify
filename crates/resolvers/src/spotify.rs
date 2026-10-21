//! Public Spotify embeds, with anonymous Web API search/fallback.
use core_types::{MetadataProvider, ResolverError, TrackMetadata};
use regex::Regex;
use reqwest::Client;
use serde_json::Value;
use tokio::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::{http, parse_error, required_string, string};
const SERVICE: &str = "Spotify";
// A public embed bootstraps a session without user credentials.
const BOOTSTRAP_TRACK: &str = "4cOdK2wGLETKBW3PvgPWqT";

pub struct Spotify {
    client: Client,
    session: Mutex<Option<(String, u64)>>,
    api_retry_at: Mutex<Option<Instant>>,
    embed_retry_at: Mutex<Option<Instant>>,
}

impl Spotify {
    pub fn new() -> Result<Self, ResolverError> {
        Ok(Self { client: http::client()?, session: Mutex::new(None),
            api_retry_at: Mutex::new(None), embed_retry_at: Mutex::new(None) })
    }

    async fn text(&self, request: reqwest::RequestBuilder, api: bool) -> Result<String, ResolverError> {
        // Each origin has its own cooldown: an API 429 must not locally block
        // open.spotify.com embeds. Both origins still respect their own 429.
        // Do not sleep in a search request or retry 429 in a tight loop.
        let mut retry_at = if api { self.api_retry_at.lock().await } else { self.embed_retry_at.lock().await };
        if let Some(seconds) = cooldown_seconds(*retry_at, Instant::now()) {
            return Err(ResolverError::RateLimited { service: SERVICE, retry_after_secs: seconds });
        }
        let result = http::text(request, SERVICE).await;
        if let Err(ResolverError::RateLimited { retry_after_secs, .. }) = &result {
            *retry_at = Instant::now().checked_add(Duration::from_secs(*retry_after_secs));
        }
        result
    }

    async fn embed(&self, kind: &str, id: &str) -> Result<Value, ResolverError> {
        let html = self.text(self.client.get(format!("https://open.spotify.com/embed/{kind}/{id}")), false).await?;
        let re = Regex::new(r#"(?s)<script\b[^>]*\bid=["']__NEXT_DATA__["'][^>]*>(.*?)</script>"#)
            .map_err(|e| parse_error(SERVICE, e.to_string()))?;
        let json = re.captures(&html).and_then(|c| c.get(1))
            .ok_or_else(|| parse_error(SERVICE, "embed has no __NEXT_DATA__"))?;
        let data: Value = serde_json::from_str(json.as_str()).map_err(|e| parse_error(SERVICE, e.to_string()))?;
        if let Some(token) = string(&data, "/props/pageProps/state/settings/session/accessToken") {
            let expires = data.pointer("/props/pageProps/state/settings/session/accessTokenExpirationTimestampMs")
                .and_then(Value::as_u64).unwrap_or(0) / 1000;
            *self.session.lock().await = Some((token, expires));
        }
        data.pointer("/props/pageProps/state/data/entity").cloned()
            .filter(|v| v.is_object()).ok_or_else(|| parse_error(SERVICE, "embed entity missing"))
    }

    async fn token(&self) -> Result<String, ResolverError> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        {
            let session = self.session.lock().await;
            if let Some((token, expires)) = session.as_ref() {
                if *expires > now + 30 { return Ok(token.clone()); }
            }
        }
        self.embed("track", BOOTSTRAP_TRACK).await?;
        self.session.lock().await.as_ref().map(|s| s.0.clone()).ok_or_else(|| ResolverError::Unavailable {
            service: SERVICE, message: "Spotify did not issue an anonymous token".into(),
        })
    }

    async fn api(&self, url: &str, query: &[(&str, &str)]) -> Result<Value, ResolverError> {
        let url = http::https_url(url, SERVICE)?;
        if url.host_str() != Some("api.spotify.com") {
            return Err(parse_error(SERVICE, "unexpected API pagination host"));
        }
        // Check before bootstrapping a token, which itself would fetch an embed.
        if let Some(seconds) = cooldown_seconds(*self.api_retry_at.lock().await, Instant::now()) {
            return Err(ResolverError::RateLimited { service: SERVICE, retry_after_secs: seconds });
        }
        for attempt in 0..2 {
            let token = self.token().await?;
            let result = self.text(self.client.get(url.clone()).bearer_auth(token).query(query), true).await
                .and_then(|body| serde_json::from_str::<Value>(&body).map_err(|e| parse_error(SERVICE, e.to_string())));
            if attempt == 0 && matches!(result, Err(ResolverError::Http { status: 401, .. })) {
                *self.session.lock().await = None;
                continue;
            }
            return result;
        }
        Err(ResolverError::Unavailable { service: SERVICE, message: "anonymous session expired".into() })
    }

    async fn track_id(&self, id: &str) -> Result<TrackMetadata, ResolverError> {
        match self.embed("track", id).await.and_then(|v| parse_track(&v)) {
            // Do not spend an anonymous API request enriching every embed.
            // Missing ISRC/album remain None rather than being inferred.
            Ok(track) => Ok(track),
            Err(error @ ResolverError::RateLimited { .. }) => Err(error),
            Err(embed_error) => {
                match self.api(&format!("https://api.spotify.com/v1/tracks/{id}"), &[]).await {
                    Ok(full) => parse_track(&full),
                    Err(error @ ResolverError::RateLimited { .. }) => Err(error),
                    Err(_) => Err(embed_error),
                }
            }
        }
    }
}


impl MetadataProvider for Spotify {
    async fn track(&self, input: &str) -> Result<TrackMetadata, ResolverError> {
        let input = input.trim();
        if let Some(id) = spotify_id(input, "track")? { return self.track_id(&id).await; }
        if input.is_empty() { return Err(ResolverError::InvalidInput("empty search query".into())); }
        let data = self.api("https://api.spotify.com/v1/search", &[("q", input), ("type", "track"), ("limit", "1")]).await?;
        let track = data.pointer("/tracks/items/0").ok_or_else(|| ResolverError::NotFound {
            service: SERVICE, query: input.into(),
        })?;
        parse_track(track)
    }

    async fn playlist(&self, input: &str) -> Result<Vec<TrackMetadata>, ResolverError> {
        let id = spotify_id(input.trim(), "playlist")?
            .ok_or_else(|| ResolverError::InvalidInput("expected Spotify playlist URL or ID".into()))?;
        let entity = self.embed("playlist", &id).await?;
        let mut next = Some(format!("https://api.spotify.com/v1/playlists/{id}/tracks?limit=100"));
        let mut tracks = Vec::new();
        let mut pages = std::collections::HashSet::new();
        let mut api_failed = false;
        while let Some(url) = next {
            if !pages.insert(url.clone()) { return Err(parse_error(SERVICE, "pagination loop")); }
            let page = match self.api(&url, &[]).await {
                Ok(page) => page,
                Err(error @ ResolverError::RateLimited { .. }) => return Err(error),
                Err(_) if tracks.is_empty() => { api_failed = true; break; }
                Err(error) => return Err(error),
            };
            let items = page["items"].as_array().ok_or_else(|| parse_error(SERVICE, "playlist items missing"))?;
            for item in items { tracks.push(parse_track(&item["track"])?); }
            next = string(&page, "/next");
            if next.is_none() && page["total"].as_u64() != Some(tracks.len() as u64) {
                return Err(parse_error(SERVICE, "incomplete API playlist"));
            }
        }
        if !api_failed { return Ok(tracks); }
        let items = entity["trackList"].as_array().ok_or_else(|| parse_error(SERVICE, "embed trackList missing"))?;
        if entity["totalTrackCount"].as_u64() != Some(items.len() as u64) {
            return Err(ResolverError::Unavailable { service: SERVICE,
                message: "embed playlist is truncated or its total is unknown; anonymous pagination denied".into() });
        }
        for item in items {
            let uri = required_string(item, "/uri", SERVICE)?;
            let id = spotify_id(&uri, "track")?.ok_or_else(|| parse_error(SERVICE, "invalid track URI"))?;
            // Fetch individual embeds for covers and unambiguous artist names.
            tracks.push(self.track_id(&id).await?);
        }
        Ok(tracks)
    }
}

fn cooldown_seconds(retry_at: Option<Instant>, now: Instant) -> Option<u64> {
    retry_at.and_then(|deadline| deadline.checked_duration_since(now))
        .filter(|remaining| !remaining.is_zero())
        .map(|remaining| remaining.as_secs().saturating_add(u64::from(remaining.subsec_nanos() > 0)))
}

#[cfg(test)]
mod cooldown_tests {
    use super::*;

    #[tokio::test]
    async fn api_limit_does_not_set_embed_limit_or_bootstrap_a_token() {
        let spotify = Spotify::new().unwrap();
        *spotify.api_retry_at.lock().await = Some(Instant::now() + Duration::from_secs(41_000));
        let result = spotify.api("https://api.spotify.com/v1/search", &[]).await;
        assert!(matches!(result, Err(ResolverError::RateLimited { retry_after_secs, .. })
            if retry_after_secs > 40_900 && retry_after_secs <= 41_000));
        assert!(spotify.embed_retry_at.lock().await.is_none());
        assert!(spotify.session.lock().await.is_none());
    }

    #[test]
    fn cooldown_rounds_up_and_expires() {
        let now = Instant::now();
        assert_eq!(cooldown_seconds(None, now), None);
        assert_eq!(cooldown_seconds(Some(now), now), None);
        assert_eq!(cooldown_seconds(Some(now + Duration::from_millis(1500)), now), Some(2));
        assert_eq!(cooldown_seconds(Some(now), now + Duration::from_secs(1)), None);
    }
}

fn spotify_id(input: &str, kind: &str) -> Result<Option<String>, ResolverError> {
    let valid = |s: &str| s.len() == 22 && s.bytes().all(|b| b.is_ascii_alphanumeric());
    if valid(input) { return Ok(Some(input.into())); }
    let candidate = if input.starts_with("spotify:") {
        input.strip_prefix(&format!("spotify:{kind}:")).map(str::to_owned)
    } else if input.contains("://") {
        let url = url::Url::parse(input).map_err(|_| ResolverError::InvalidInput("invalid Spotify URL".into()))?;
        if url.scheme() != "https" || url.host_str() != Some("open.spotify.com") {
            return Err(ResolverError::InvalidInput("expected https://open.spotify.com URL".into()));
        }
        let parts: Vec<_> = url.path_segments().into_iter().flatten().collect();
        parts.windows(2).find(|pair| pair[0] == kind).map(|pair| pair[1].to_owned())
    } else { return Ok(None); };
    candidate.filter(|s| valid(s)).map(Some)
        .ok_or_else(|| ResolverError::InvalidInput(format!("expected Spotify {kind} ID")))
}

fn parse_track(value: &Value) -> Result<TrackMetadata, ResolverError> {
    let uri = required_string(value, "/uri", SERVICE)?;
    if !uri.starts_with("spotify:track:") { return Err(parse_error(SERVICE, "not a Spotify track")); }
    let title = string(value, "/title").or_else(|| string(value, "/name"))
        .ok_or_else(|| parse_error(SERVICE, "title missing"))?;
    let duration_ms = value["duration_ms"].as_u64().or_else(|| value["duration"].as_u64())
        .filter(|d| *d > 0).ok_or_else(|| parse_error(SERVICE, "exact duration missing"))?;
    let artists: Vec<String> = value["artists"].as_array().into_iter().flatten()
        .filter_map(|artist| string(artist, "/name")).collect();
    if artists.is_empty() { return Err(parse_error(SERVICE, "artists missing")); }
    let cover_url = ["/album/images", "/visualIdentity/image", "/coverArt/sources"]
        .iter().filter_map(|p| value.pointer(p).and_then(Value::as_array))
        .flatten().filter(|image| image["url"].is_string())
        .max_by_key(|image| {
            let w = image["width"].as_u64().or_else(|| image["maxWidth"].as_u64()).unwrap_or(0);
            let h = image["height"].as_u64().or_else(|| image["maxHeight"].as_u64()).unwrap_or(0);
            w.saturating_mul(h)
        }).and_then(|image| string(image, "/url"));
    Ok(TrackMetadata {
        id: uri, title, artists, duration_ms, cover_url,
        album: string(value, "/album/name"),
        isrc: string(value, "/external_ids/isrc").or_else(|| string(value, "/isrc")),
    })
}
