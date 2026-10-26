//! Public Spotify embeds, with anonymous Web API search/fallback.
//! Anonymous sessions are not official OAuth or Client Credentials authorization.
use core_types::{MetadataProvider, ResolverError, TrackMetadata};
use regex::Regex;
use reqwest::Client;
use serde_json::Value;
use tokio::sync::Mutex;
use std::{collections::HashMap, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};

use crate::{http, parse_error, required_string, string};
const SERVICE: &str = "Spotify";
// A public embed bootstraps a session without user credentials.
const BOOTSTRAP_TRACK: &str = "4cOdK2wGLETKBW3PvgPWqT";
const METADATA_TTL: Duration = Duration::from_secs(5 * 60);
const METADATA_CAPACITY: usize = 64;
const EMBED: &str = "embed";

/// Spotify rate-limits anonymous Web API endpoints independently: `/v1/tracks`
/// can return a day-long 429 while `/v1/search` still answers. One shared
/// cooldown would block every working endpoint with the longest limit.
fn api_bucket(url: &url::Url) -> &'static str {
    match url.path_segments().and_then(|mut s| s.nth(1)) {
        Some("search") => "api:search",
        Some("tracks") => "api:tracks",
        Some("playlists") => "api:playlists",
        _ => "api:other",
    }
}

#[derive(Default)]
struct MetadataCache(Vec<(String, Instant, TrackMetadata)>);

impl MetadataCache {
    fn get(&mut self, key: &str, now: Instant) -> Option<TrackMetadata> {
        self.0.retain(|(_, inserted, _)| now.duration_since(*inserted) < METADATA_TTL);
        self.0.iter().find(|(cached, _, _)| cached == key).map(|(_, _, track)| track.clone())
    }

    fn insert(&mut self, key: String, track: TrackMetadata, now: Instant) {
        self.0.retain(|(cached, inserted, _)| cached != &key && now.duration_since(*inserted) < METADATA_TTL);
        if self.0.len() == METADATA_CAPACITY { self.0.remove(0); }
        self.0.push((key, now, track));
    }
}

pub struct Spotify {
    client: Client,
    session: Mutex<Option<(String, u64)>>,
    session_refresh: Mutex<()>,
    /// Retry deadlines keyed by `EMBED` or an `api_bucket`.
    cooldowns: Mutex<HashMap<&'static str, Instant>>,
    metadata: Mutex<MetadataCache>,
}

impl Spotify {
    pub fn new() -> Result<Self, ResolverError> {
        Ok(Self { client: http::client()?, session: Mutex::new(None), session_refresh: Mutex::new(()),
            cooldowns: Mutex::new(HashMap::new()), metadata: Mutex::new(MetadataCache::default()) })
    }

    async fn check_cooldown(&self, bucket: &'static str) -> Result<(), ResolverError> {
        match cooldown_seconds(self.cooldowns.lock().await.get(bucket).copied(), Instant::now()) {
            Some(seconds) => Err(ResolverError::RateLimited { service: SERVICE, retry_after_secs: seconds }),
            None => Ok(()),
        }
    }

    async fn set_cooldown(&self, bucket: &'static str, duration: Duration) {
        if let Some(deadline) = Instant::now().checked_add(duration) {
            let mut cooldowns = self.cooldowns.lock().await;
            let entry = cooldowns.entry(bucket).or_insert(deadline);
            *entry = (*entry).max(deadline);
        }
    }

    async fn text(&self, request: reqwest::RequestBuilder, bucket: &'static str) -> Result<String, ResolverError> {
        // Respect each endpoint's own 429 without blocking the others.
        // Do not sleep in a search request or retry 429 in a tight loop.
        self.check_cooldown(bucket).await?;
        let result = http::text(request, SERVICE).await;
        if let Err(ResolverError::RateLimited { retry_after_secs, .. }) = &result {
            self.set_cooldown(bucket, Duration::from_secs(*retry_after_secs)).await;
        }
        result
    }

    async fn embed(&self, kind: &str, id: &str) -> Result<Value, ResolverError> {
        let data = self.embed_data(kind, id).await?;
        embed_entity(&data)
    }

    async fn embed_data(&self, kind: &str, id: &str) -> Result<Value, ResolverError> {
        let html = self.text(self.client.get(format!("https://open.spotify.com/embed/{kind}/{id}")), EMBED).await?;
        self.parse_embed(&html).await
    }

    async fn parse_embed(&self, html: &str) -> Result<Value, ResolverError> {
        let re = Regex::new(r#"(?s)<script[^>]*id=["']?__NEXT_DATA__["']?[^>]*>(.*?)</script>"#)
            .map_err(|e| parse_error(SERVICE, e.to_string()))?;
        let json = re.captures(&html).and_then(|c| c.get(1))
            .ok_or_else(|| parse_error(SERVICE, "embed has no __NEXT_DATA__"))?;
        let data: Value = serde_json::from_str(json.as_str()).map_err(|e| parse_error(SERVICE, e.to_string()))?;
        // Spotify can serve an error page with HTTP 200 and no state/entity.
        if let Some(status) = data.pointer("/props/pageProps/status").and_then(Value::as_u64)
            .filter(|status| (400..600).contains(status)) {
            if status == 429 {
                self.set_cooldown(EMBED, Duration::from_secs(60)).await;
                return Err(ResolverError::RateLimited { service: SERVICE, retry_after_secs: 60 });
            }
            return Err(ResolverError::Http { service: SERVICE, status: status as u16 });
        }
        if let Some(token) = string(&data, "/props/pageProps/state/settings/session/accessToken") {
            let expires = data.pointer("/props/pageProps/state/settings/session/accessTokenExpirationTimestampMs")
                .and_then(Value::as_u64).unwrap_or(0) / 1000;
            let candidate = Some((token, expires));
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
            let mut session = self.session.lock().await;
            if session_token(&candidate, now).is_some()
                && session.as_ref().map_or(true, |current| expires > current.1) {
                *session = candidate;
            }
        }
        Ok(data)
    }

    async fn token(&self) -> Result<String, ResolverError> {
        // Recheck the cache after acquiring the lock so concurrent misses share a bootstrap.
        let _refresh = self.session_refresh.lock().await;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        {
            let session = self.session.lock().await;
            if let Some(token) = session_token(&session, now) { return Ok(token); }
        }
        // A session bootstrap needs no metadata entity.
        self.embed_data("track", BOOTSTRAP_TRACK).await?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        session_token(&*self.session.lock().await, now).ok_or_else(|| ResolverError::Unavailable {
            service: SERVICE, message: "Spotify did not issue a usable anonymous token".into(),
        })
    }

    async fn invalidate_token(&self, rejected: &str) {
        let mut session = self.session.lock().await;
        // A late 401 for an older request must not discard a refreshed session.
        if session.as_ref().is_some_and(|(token, _)| token == rejected) {
            *session = None;
        }
    }

    async fn api(&self, url: &str, query: &[(&str, &str)]) -> Result<Value, ResolverError> {
        let url = http::https_url(url, SERVICE)?;
        if url.host_str() != Some("api.spotify.com") {
            return Err(parse_error(SERVICE, "unexpected API pagination host"));
        }
        let bucket = api_bucket(&url);
        // Check before bootstrapping a token, which itself would fetch an embed.
        self.check_cooldown(bucket).await?;
        for attempt in 0..2 {
            let token = self.token().await?;
            let result = self.text(self.client.get(url.clone()).bearer_auth(&token).query(query), bucket).await
                .and_then(|body| serde_json::from_str::<Value>(&body).map_err(|e| parse_error(SERVICE, e.to_string())));
            if matches!(result, Err(ResolverError::Http { status: 401, .. })) {
                self.invalidate_token(&token).await;
                if attempt == 0 { continue; }
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
            Err(error @ ResolverError::Http { status: 451, .. }) => Err(error),
            Err(embed_error) => {
                match self.api(&format!("https://api.spotify.com/v1/tracks/{id}"), &[]).await {
                    Ok(full) => parse_track(&full),
                    Err(error @ ResolverError::RateLimited { .. }) => Err(error),
                    Err(error @ ResolverError::Http { status: 451, .. }) => Err(error),
                    Err(_) => Err(embed_error),
                }
            }
        }
    }
}


impl MetadataProvider for Spotify {
    async fn track(&self, input: &str) -> Result<TrackMetadata, ResolverError> {
        let input = input.trim();
        let id = spotify_id(input, "track")?;
        if input.is_empty() { return Err(ResolverError::InvalidInput("empty search query".into())); }
        let key = match &id {
            Some(id) => format!("track:{id}"),
            None => format!("query:{input}"),
        };
        // Hold through resolution so waiters recheck successful metadata before any I/O.
        let mut cache = self.metadata.lock().await;
        if let Some(track) = cache.get(&key, Instant::now()) { return Ok(track); }
        let track = if let Some(id) = id {
            self.track_id(&id).await?
        } else {
            let data = self.api("https://api.spotify.com/v1/search", &[("q", input), ("type", "track"), ("limit", "1")]).await?;
            let track = data.pointer("/tracks/items/0").ok_or_else(|| ResolverError::NotFound {
                service: SERVICE, query: input.into(),
            })?;
            parse_track(track)?
        };
        cache.insert(key, track.clone(), Instant::now());
        Ok(track)
    }

    async fn playlist(&self, input: &str) -> Result<Vec<TrackMetadata>, ResolverError> {
        let id = spotify_id(input.trim(), "playlist")?
            .ok_or_else(|| ResolverError::InvalidInput("expected Spotify playlist URL or ID".into()))?;
        // One embed request already lists the tracks with exact durations.
        // The anonymous Web API only adds pagination and is often limited for
        // hours, so its failure falls back to the embed list instead of failing.
        let entity = self.embed("playlist", &id).await?;
        let embedded = embed_playlist_tracks(&entity)?;
        match self.api_playlist(&id).await {
            Ok(tracks) if tracks.len() >= embedded.len() => Ok(tracks),
            Err(error @ ResolverError::Http { status: 451, .. }) => Err(error),
            _ if !embedded.is_empty() => Ok(embedded),
            Ok(tracks) => Ok(tracks),
            Err(error) => Err(error),
        }
    }
}

impl Spotify {
    async fn api_playlist(&self, id: &str) -> Result<Vec<TrackMetadata>, ResolverError> {
        let mut next = Some(format!("https://api.spotify.com/v1/playlists/{id}/tracks?limit=100"));
        let mut tracks = Vec::new();
        let mut pages = std::collections::HashSet::new();
        while let Some(url) = next {
            if !pages.insert(url.clone()) { return Err(parse_error(SERVICE, "pagination loop")); }
            let page = self.api(&url, &[]).await?;
            let items = page["items"].as_array().ok_or_else(|| parse_error(SERVICE, "playlist items missing"))?;
            // Local files and removed tracks have no usable metadata; skip them.
            tracks.extend(items.iter().filter_map(|item| parse_track(&item["track"]).ok()));
            next = string(&page, "/next");
        }
        Ok(tracks)
    }
}

/// Embed playlist rows carry uri, title, a ", "-joined artist subtitle and
/// an exact duration, but no per-track cover. Unplayable rows are skipped.
fn embed_playlist_tracks(entity: &Value) -> Result<Vec<TrackMetadata>, ResolverError> {
    let items = entity["trackList"].as_array().ok_or_else(|| parse_error(SERVICE, "embed trackList missing"))?;
    Ok(items.iter().filter(|item| item["isPlayable"].as_bool() != Some(false)).filter_map(|item| {
        let id = required_string(item, "/uri", SERVICE).ok().filter(|uri| uri.starts_with("spotify:track:"))?;
        let artists: Vec<String> = string(item, "/subtitle")?.split(',')
            .map(|a| a.trim_matches(|c: char| c.is_whitespace()).to_owned()).filter(|a| !a.is_empty()).collect();
        let duration_ms = item["duration"].as_u64().filter(|d| *d > 0)?;
        Some(TrackMetadata { id, title: string(item, "/title")?, artists, duration_ms,
            album: None, cover_url: None, isrc: None })
    }).filter(|track| !track.artists.is_empty()).collect())
}

fn session_token(session: &Option<(String, u64)>, now: u64) -> Option<String> {
    session.as_ref().filter(|(token, expires)| !token.trim().is_empty() && *expires > now.saturating_add(30))
        .map(|(token, _)| token.clone())
}

fn cooldown_seconds(retry_at: Option<Instant>, now: Instant) -> Option<u64> {
    retry_at.and_then(|deadline| deadline.checked_duration_since(now))
        .filter(|remaining| !remaining.is_zero())
        .map(|remaining| remaining.as_secs().saturating_add(u64::from(remaining.subsec_nanos() > 0)))
}

fn embed_entity(data: &Value) -> Result<Value, ResolverError> {
    data.pointer("/props/pageProps/state/data/entity").cloned()
        .filter(|v| v.is_object()).ok_or_else(|| parse_error(SERVICE, "embed entity missing"))
}

#[cfg(test)]
mod metadata_cache_tests {
    use super::*;

    fn fixture() -> TrackMetadata {
        parse_track(&serde_json::json!({
            "uri": format!("spotify:track:{BOOTSTRAP_TRACK}"), "name": "Fixture",
            "duration_ms": 123000, "artists": [{"name": "Artist"}]
        })).unwrap()
    }

    #[test]
    fn ttl_is_fixed_and_expires_at_boundary() {
        let now = Instant::now();
        let mut cache = MetadataCache::default();
        cache.insert("key".into(), fixture(), now);
        assert!(cache.get("key", now + METADATA_TTL - Duration::from_nanos(1)).is_some());
        assert!(cache.get("key", now + METADATA_TTL).is_none());
        assert!(cache.0.is_empty());
    }

    #[test]
    fn capacity_evicts_oldest_and_replacement_does_not_grow() {
        let now = Instant::now();
        let mut cache = MetadataCache::default();
        for i in 0..METADATA_CAPACITY {
            cache.insert(i.to_string(), fixture(), now);
        }
        cache.insert("0".into(), fixture(), now);
        assert_eq!(cache.0.len(), METADATA_CAPACITY);
        cache.insert("extra".into(), fixture(), now);
        assert_eq!(cache.0.len(), METADATA_CAPACITY);
        assert!(cache.get("1", now).is_none());
        assert!(cache.get("0", now).is_some());
        cache.insert("fresh".into(), fixture(), now + METADATA_TTL);
        assert_eq!(cache.0.len(), 1);
    }

    #[tokio::test]
    async fn normalized_hits_work_during_cooldown_and_errors_are_not_cached() {
        let spotify = Spotify::new().unwrap();
        let now = Instant::now();
        spotify.set_cooldown("api:search", Duration::from_secs(41000)).await;
        spotify.set_cooldown(EMBED, Duration::from_secs(41000)).await;
        for input in [BOOTSTRAP_TRACK, "Exact Query"] {
            assert!(matches!(spotify.track(input).await, Err(ResolverError::RateLimited { .. })));
        }
        assert!(spotify.metadata.lock().await.0.is_empty());
        {
            let mut cache = spotify.metadata.lock().await;
            cache.insert(format!("track:{BOOTSTRAP_TRACK}"), fixture(), now);
            cache.insert("query:Exact Query".into(), fixture(), now);
        }
        for input in [
            format!(" {BOOTSTRAP_TRACK} "),
            format!("spotify:track:{BOOTSTRAP_TRACK}"),
            format!("https://open.spotify.com/track/{BOOTSTRAP_TRACK}?si=fixture"),
            " Exact Query ".into(),
        ] {
            assert_eq!(spotify.track(&input).await.unwrap().title, "Fixture");
        }
        for input in ["exact query", "Exact  Query"] {
            assert!(matches!(spotify.track(input).await, Err(ResolverError::RateLimited { .. })));
        }
        assert_eq!(spotify.metadata.lock().await.0.len(), 2);
        assert!(spotify.session.lock().await.is_none());
    }

    #[tokio::test]
    async fn concurrent_waiters_recheck_metadata_after_resolution() {
        let spotify = std::sync::Arc::new(Spotify::new().unwrap());
        spotify.set_cooldown(EMBED, Duration::from_secs(41000)).await;
        let mut cache = spotify.metadata.lock().await;
        let mut waiters = Vec::new();
        for _ in 0..8 {
            let spotify = spotify.clone();
            waiters.push(tokio::spawn(async move { spotify.track(BOOTSTRAP_TRACK).await }));
        }
        tokio::task::yield_now().await;
        assert!(waiters.iter().all(|waiter| !waiter.is_finished()));
        cache.insert(format!("track:{BOOTSTRAP_TRACK}"), fixture(), Instant::now());
        drop(cache);
        for waiter in waiters {
            assert_eq!(waiter.await.unwrap().unwrap().title, "Fixture");
        }
    }
}

#[cfg(test)]
mod cooldown_tests {
    use super::*;

    #[tokio::test]
    async fn api_limit_does_not_set_embed_limit_or_bootstrap_a_token() {
        let spotify = Spotify::new().unwrap();
        spotify.set_cooldown("api:search", Duration::from_secs(41_000)).await;
        let result = spotify.api("https://api.spotify.com/v1/search", &[]).await;
        assert!(matches!(result, Err(ResolverError::RateLimited { retry_after_secs, .. })
            if retry_after_secs > 40_900 && retry_after_secs <= 41_000));
        assert!(!spotify.cooldowns.lock().await.contains_key(EMBED));
        assert!(spotify.session.lock().await.is_none());
    }

    #[tokio::test]
    async fn day_long_tracks_limit_does_not_block_search() {
        // Observed: /v1/tracks 429 with Retry-After ~76000 while /v1/search answered 200.
        let spotify = Spotify::new().unwrap();
        spotify.set_cooldown("api:tracks", Duration::from_secs(76_000)).await;
        spotify.set_cooldown("api:playlists", Duration::from_secs(76_000)).await;
        assert!(spotify.check_cooldown("api:search").await.is_ok());
        assert!(spotify.check_cooldown(EMBED).await.is_ok());
        assert!(matches!(spotify.check_cooldown("api:tracks").await, Err(ResolverError::RateLimited { .. })));
        // A shorter later limit never shortens an existing deadline.
        spotify.set_cooldown("api:tracks", Duration::from_secs(5)).await;
        assert!(matches!(spotify.check_cooldown("api:tracks").await,
            Err(ResolverError::RateLimited { retry_after_secs, .. }) if retry_after_secs > 75_000));
    }

    #[test]
    fn api_buckets_follow_endpoint() {
        for (url, bucket) in [
            ("https://api.spotify.com/v1/search?q=a", "api:search"),
            ("https://api.spotify.com/v1/tracks/x", "api:tracks"),
            ("https://api.spotify.com/v1/playlists/x/tracks?limit=100", "api:playlists"),
            ("https://api.spotify.com/v1/albums/x", "api:other"),
        ] {
            assert_eq!(api_bucket(&url::Url::parse(url).unwrap()), bucket);
        }
    }

    #[test]
    fn embed_playlist_rows_become_tracks() {
        // Sanitized shape of a live playlist embed row (no totalTrackCount field).
        let entity = serde_json::json!({"trackList": [
            {"uri": "spotify:track:3301sy6RP2XMn67qrjzY0y", "title": "Lil Krazy",
             "subtitle": "Yeat,\u{a0}Lyfestyle Corporation", "duration": 165000, "isPlayable": true},
            {"uri": "spotify:track:2sFepQNyl1WjpROruP8bJ0", "title": "Housewife", "subtitle": "Tate McRae", "duration": 208810},
            {"uri": "spotify:track:unplayable000000000000", "title": "Gone", "subtitle": "A", "duration": 1000, "isPlayable": false},
            {"uri": "spotify:episode:x", "title": "Podcast", "subtitle": "B", "duration": 1000},
            {"uri": "spotify:track:noduration00000000000", "title": "X", "subtitle": "C"}
        ]});
        let tracks = embed_playlist_tracks(&entity).unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].artists, ["Yeat", "Lyfestyle Corporation"]);
        assert_eq!(tracks[0].duration_ms, 165000);
        assert_eq!(tracks[1].id, "spotify:track:2sFepQNyl1WjpROruP8bJ0");
        assert!(matches!(embed_playlist_tracks(&serde_json::json!({})), Err(ResolverError::Parse { .. })));
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

#[cfg(test)]
mod embed_tests {
    use super::*;
    use serde_json::json;

    fn html(data: Value) -> String {
        format!("<html><script type='application/json' id='__NEXT_DATA__'>{data}</script></html>")
    }

    #[test]
    fn cached_token_requires_nonempty_value_and_expiry_margin() {
        assert_eq!(session_token(&None, 100), None);
        for (token, expires) in [("token", 0), ("token", 99), ("token", 130), ("", 200), ("  ", 200)] {
            assert_eq!(session_token(&Some((token.into(), expires)), 100), None);
        }
        assert_eq!(session_token(&Some(("token".into(), 131)), 100).as_deref(), Some("token"));
        assert_eq!(session_token(&Some(("token".into(), u64::MAX)), u64::MAX), None);
    }

    #[tokio::test]
    async fn invalid_or_older_embed_sessions_do_not_replace_valid_cache() {
        let spotify = Spotify::new().unwrap();
        for initial in [None, Some(("newer".into(), 4102444800))] {
            *spotify.session.lock().await = initial.clone();
            for session in [
                json!({"accessToken": "missing-expiry"}),
                json!({"accessToken": "expired", "accessTokenExpirationTimestampMs": 1000}),
                json!({"accessToken": "", "accessTokenExpirationTimestampMs": 4102444800000u64}),
                json!({"accessToken": "bad-expiry", "accessTokenExpirationTimestampMs": "invalid"}),
                json!({}),
            ] {
                spotify.parse_embed(&html(json!({"props": {"pageProps": {"state": {
                    "settings": {"session": session}
                }}}}))).await.unwrap();
                assert_eq!(*spotify.session.lock().await, initial);
            }
        }
        spotify.parse_embed(&html(json!({"props": {"pageProps": {"state": {
            "settings": {"session": {
                "accessToken": "older", "accessTokenExpirationTimestampMs": 4102444700000u64
            }}
        }}}}))).await.unwrap();
        assert_eq!(spotify.token().await.unwrap(), "newer");
    }

    #[tokio::test]
    async fn rejected_token_invalidation_preserves_newer_session() {
        let spotify = Spotify::new().unwrap();
        *spotify.session.lock().await = Some(("newer".into(), 4102444800));
        spotify.invalidate_token("older").await;
        assert_eq!(spotify.token().await.unwrap(), "newer");
        spotify.invalidate_token("newer").await;
        assert!(spotify.session.lock().await.is_none());
        spotify.invalidate_token("newer").await;
        assert!(spotify.session.lock().await.is_none());
    }

    #[tokio::test]
    async fn expired_cache_cannot_bypass_bootstrap_restrictions() {
        let spotify = Spotify::new().unwrap();
        *spotify.session.lock().await = Some(("expired".into(), 1));
        spotify.set_cooldown(EMBED, Duration::from_secs(60)).await;
        assert!(matches!(spotify.token().await, Err(ResolverError::RateLimited { .. })));
    }

    #[tokio::test]
    async fn concurrent_waiters_recheck_cache_after_refresh() {
        let spotify = std::sync::Arc::new(Spotify::new().unwrap());
        // Make any accidental bootstrap fail locally rather than contacting Spotify.
        spotify.set_cooldown(EMBED, Duration::from_secs(60)).await;
        let refresh = spotify.session_refresh.lock().await;
        let mut waiters = Vec::new();
        for _ in 0..8 {
            let spotify = spotify.clone();
            waiters.push(tokio::spawn(async move { spotify.token().await }));
        }
        tokio::task::yield_now().await;
        assert!(waiters.iter().all(|waiter| !waiter.is_finished()));
        *spotify.session.lock().await = Some(("refreshed".into(), 4102444800));
        drop(refresh);
        for waiter in waiters {
            assert_eq!(waiter.await.unwrap().unwrap(), "refreshed");
        }
    }

    #[tokio::test]
    async fn token_bootstrap_does_not_require_an_entity() {
        let spotify = Spotify::new().unwrap();
        let data = spotify.parse_embed(&html(json!({"props": {"pageProps": {"state": {
            "settings": {"session": {
                "accessToken": "fixture-token", "accessTokenExpirationTimestampMs": 4102444800000u64
            }}
        }}}}))).await.unwrap();
        // The bootstrap succeeds and the cached token is usable without network I/O.
        assert_eq!(spotify.token().await.unwrap(), "fixture-token");
        assert_eq!(spotify.session.lock().await.as_ref().unwrap().1, 4102444800);
        assert!(matches!(embed_entity(&data), Err(ResolverError::Parse { .. })));
    }

    #[tokio::test]
    async fn entity_without_session_still_resolves_metadata() {
        let spotify = Spotify::new().unwrap();
        let data = spotify.parse_embed(&html(json!({"props": {"pageProps": {"state": {
            "data": {"entity": {
                "uri": "spotify:track:4cOdK2wGLETKBW3PvgPWqT",
                "title": "Fixture title", "duration": 213000,
                "artists": [{"name": "Fixture artist"}],
                "visualIdentity": {"image": [
                    {"url": "https://example.com/small.jpg", "maxWidth": 64, "maxHeight": 64},
                    {"url": "https://example.com/large.jpg", "maxWidth": 640, "maxHeight": 640}
                ]}
            }}
        }}}}))).await.unwrap();
        let track = parse_track(&embed_entity(&data).unwrap()).unwrap();
        assert_eq!(track.title, "Fixture title");
        assert_eq!(track.artists, ["Fixture artist"]);
        assert_eq!(track.duration_ms, 213000);
        assert_eq!(track.cover_url.as_deref(), Some("https://example.com/large.jpg"));
        assert!(track.album.is_none());
        assert!(track.isrc.is_none());
        assert!(spotify.session.lock().await.is_none());
    }

    #[tokio::test]
    async fn country_error_page_preserves_embedded_status() {
        let spotify = Spotify::new().unwrap();
        // Sanitized shape observed on the public embed, despite HTTP 200.
        let result = spotify.parse_embed(&html(json!({"props": {"pageProps": {
            "status": 451, "title": "Sorry, that's not currently available.",
            "description": "This content is not available in your country.",
            "links": [], "rtl": false
        }}}))).await;
        assert!(matches!(result, Err(ResolverError::Http { service: SERVICE, status: 451 })));
        assert!(spotify.session.lock().await.is_none());
    }

    #[tokio::test]
    async fn embedded_errors_do_not_cache_tokens_and_429_sets_only_embed_cooldown() {
        for status in [401, 403, 404, 429, 500] {
            let spotify = Spotify::new().unwrap();
            let result = spotify.parse_embed(&html(json!({"props": {"pageProps": {
                "status": status,
                "state": {"settings": {"session": {"accessToken": "fixture-token"}}}
            }}}))).await;
            if status == 429 {
                assert!(matches!(result, Err(ResolverError::RateLimited { retry_after_secs: 60, .. })));
                assert!(spotify.cooldowns.lock().await.contains_key(EMBED));
                let result = spotify.embed("track", BOOTSTRAP_TRACK).await;
                assert!(matches!(result, Err(ResolverError::RateLimited { .. })));
            } else {
                assert!(matches!(result, Err(ResolverError::Http { status: actual, .. }) if actual == status));
                assert!(!spotify.cooldowns.lock().await.contains_key(EMBED));
            }
            assert!(!spotify.cooldowns.lock().await.keys().any(|k| k.starts_with("api")));
            assert!(spotify.session.lock().await.is_none());
        }
    }

    #[tokio::test]
    async fn malformed_embeds_remain_parse_errors() {
        let spotify = Spotify::new().unwrap();
        for html in ["<html></html>", "<script id=\"__NEXT_DATA__\">{</script>"] {
            assert!(matches!(spotify.parse_embed(html).await, Err(ResolverError::Parse { .. })));
        }
        for entity in [Value::Null, json!([]), json!("not an entity")] {
            let data = spotify.parse_embed(&html(json!({"props": {"pageProps": {
                "state": {"data": {"entity": entity}}
            }}}))).await.unwrap();
            let error = embed_entity(&data).unwrap_err();
            assert_eq!(error.to_string(), "Spotify: invalid response: embed entity missing");
        }
        assert!(spotify.session.lock().await.is_none());
    }

    #[test]
    fn api_track_shape_and_required_metadata_are_preserved() {
        let value = json!({
            "uri": "spotify:track:4cOdK2wGLETKBW3PvgPWqT", "name": "Fixture title",
            "duration_ms": 213000, "artists": [{"name": "Fixture artist"}],
            "album": {"name": "Fixture album", "images": []},
            "external_ids": {"isrc": "FIXTURE"}
        });
        let track = parse_track(&value).unwrap();
        assert_eq!(track.album.as_deref(), Some("Fixture album"));
        assert_eq!(track.isrc.as_deref(), Some("FIXTURE"));
        for field in ["uri", "name", "duration_ms", "artists"] {
            let mut incomplete = value.clone();
            incomplete.as_object_mut().unwrap().remove(field);
            assert!(matches!(parse_track(&incomplete), Err(ResolverError::Parse { .. })));
        }
    }

    #[test]
    fn text_queries_are_distinguished_from_track_references() {
        for text in ["daft punk one more time", " artist - title ", "spotify"] { assert!(is_text_query(text), "{text}"); }
        for reference in ["4cOdK2wGLETKBW3PvgPWqT", "spotify:track:4cOdK2wGLETKBW3PvgPWqT",
            "https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT?si=x", "spotify:album:4cOdK2wGLETKBW3PvgPWqT",
            "https://example.com/track/x"] { assert!(!is_text_query(reference), "{reference}"); }
    }
}

/// True for free-text search input; false for a Spotify track URL, URI or ID
/// (or malformed Spotify links), whose metadata no other provider can supply.
pub fn is_text_query(input: &str) -> bool {
    matches!(spotify_id(input.trim(), "track"), Ok(None))
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
