use core_types::{AudioCodec, MetadataProvider, ResolverError, StreamInfo, StreamResolver, StreamSource, TrackMetadata};
use regex::Regex;
use serde_json::Value;
use tokio::sync::Mutex;
use crate::{duration_matches, expires_at, http, parse_error, queries, required_string, string};

const SERVICE: &str = "SoundCloud";
const API: &str = "https://api-v2.soundcloud.com";
const HOMEPAGE: &str = "SoundCloud [client discovery: homepage]";
const SCRIPT: &str = "SoundCloud [client discovery: script]";
const LOOKUP: &str = "SoundCloud [track lookup]";
const RESOLVE: &str = "SoundCloud [URL resolve]";
const SEARCH: &str = "SoundCloud [track search]";
const TRANSCODING: &str = "SoundCloud [transcoding lookup]";
const MEDIA: &str = "SoundCloud [media availability probe]";
const RELATED: &str = "SoundCloud [related tracks]";
const STATION: &str = "SoundCloud [track station]";
const HYDRATE: &str = "SoundCloud [track hydration]";
/// Below this many playable `/related` hits, top up from the track station.
const RELATED_MIN: usize = 5;

pub struct SoundCloud {
    client: reqwest::Client,
    client_id: Mutex<Option<String>>,
    native_only: bool,
}

impl SoundCloud {
    pub fn new() -> Result<Self, ResolverError> {
        Ok(Self { client: http::client()?, client_id: Mutex::new(None), native_only: false })
    }

    /// Progressive MP3 only: native playback must not select AAC, Opus or HLS.
    pub fn new_native() -> Result<Self, ResolverError> {
        let mut resolver = Self::new()?;
        resolver.native_only = true;
        Ok(resolver)
    }

    async fn client_id(&self) -> Result<String, ResolverError> {
        let mut cached = self.client_id.lock().await;
        if let Some(id) = cached.as_ref() { return Ok(id.clone()); }
        let html = http::text(self.client.get("https://soundcloud.com/"), HOMEPAGE).await?;
        let urls = public_script_urls(&html)?;
        let mut last_error = None;
        for url in urls.into_iter().rev() {
            let script = match http::text(self.client.get(url), SCRIPT).await {
                Ok(script) => script,
                Err(error @ ResolverError::RateLimited { .. }) => return Err(error),
                Err(error) => { last_error = Some(error); continue; }
            };
            if let Some(found) = public_client_id(&script)? {
                let id = found.to_owned();
                *cached = Some(id.clone());
                return Ok(id);
            }
        }
        Err(last_error.unwrap_or_else(|| ResolverError::Unavailable {
            service: SCRIPT, message: "public client_id not found in scripts".into(),
        }))
    }

    async fn api(&self, raw: &str, params: &[(&str, &str)], stage: &'static str) -> Result<Value, ResolverError> {
        let mut url = http::https_url(raw, stage)?;
        if url.host_str() != Some("api-v2.soundcloud.com") {
            return Err(parse_error(stage, "unexpected API host"));
        }
        let pairs: Vec<(String, String)> = url.query_pairs().filter(|(k, _)| k != "client_id")
            .map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
        url.set_query(None);
        url.query_pairs_mut().extend_pairs(pairs);
        for attempt in 0..2 {
            let id = self.client_id().await?;
            let result = http::json(self.client.get(url.clone()).query(params).query(&[("client_id", id)]), stage).await;
            if attempt == 0 && matches!(result, Err(ResolverError::Http { status: 401 | 403, .. })) {
                *self.client_id.lock().await = None;
                continue;
            }
            return result;
        }
        Err(ResolverError::Unavailable { service: stage, message: "client_id rejected".into() })
    }

    async fn lookup(&self, input: &str) -> Result<Value, ResolverError> {
        if input.starts_with("soundcloud:track:") {
            let (id, _) = track_digits(input)?;
            return self.api(&format!("{API}/tracks/{id}"), &[], LOOKUP).await;
        }
        let url = http::https_url(input, SERVICE)?;
        if !matches!(url.host_str(), Some("soundcloud.com" | "www.soundcloud.com" | "m.soundcloud.com")) {
            return Err(ResolverError::InvalidInput("expected public SoundCloud URL".into()));
        }
        self.api(&format!("{API}/resolve"), &[("url", input)], RESOLVE).await
    }

    async fn stream(&self, value: &Value, expected_ms: u64) -> Result<StreamInfo, ResolverError> {
        if restricted(value) {
            return Err(ResolverError::Unavailable { service: SERVICE, message: "track is restricted or not streamable".into() });
        }
        let transcodings = value.pointer("/media/transcodings").and_then(Value::as_array)
            .ok_or_else(|| parse_error(SERVICE, "transcodings missing"))?;
        let mut formats: Vec<_> = transcodings.iter().filter_map(|t| {
            if !duration_matches(expected_ms, t["duration"].as_u64()?) { return None; }
            let (codec, protocol) = transcoding_format(t, self.native_only)?;
            Some((t, codec, protocol))
        }).collect();
        formats.sort_by_key(|(_, codec, protocol)| (
            if *codec == AudioCodec::Opus { 0 } else { 1 },
            if *protocol == "progressive" { 0 } else { 1 },
        ));
        let auth = string(value, "/track_authorization");
        let mut last_error = None;
        for (format, codec, protocol) in formats {
            let hls = protocol == "hls";
            let Some(endpoint) = format["url"].as_str() else { continue };
            let params: Vec<_> = auth.as_deref().map(|a| ("track_authorization", a)).into_iter().collect();
            let result = match self.api(endpoint, &params, TRANSCODING).await {
                Ok(result) => result,
                Err(error @ ResolverError::RateLimited { .. }) => return Err(error),
                Err(error) => { last_error = Some(error); continue; }
            };
            let Some(url) = string(&result, "/url") else {
                last_error = Some(parse_error(TRANSCODING, "media URL missing"));
                continue;
            };
            if let Err(error) = http::https_url(&url, TRANSCODING) {
                last_error = Some(error);
                continue;
            }
            let probe = if hls { http::probe_playlist(&self.client, &url, MEDIA).await }
                else { http::probe_media(&self.client, &url, MEDIA, self.native_only).await };
            match probe {
                Ok(_) => return Ok(StreamInfo {
                    expires_at: expires_at(&url), stream_url: url, codec, hls,
                    bitrate: format["bitrate"].as_u64().and_then(|b| u32::try_from(b).ok()),
                    source: StreamSource::SoundCloud,
                }),
                Err(error @ ResolverError::RateLimited { .. }) => return Err(error),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| ResolverError::Unavailable {
            service: SERVICE, message: if self.native_only {
                "no full-length AAC/MP3 stream within duration tolerance; Opus is not supported by the native decoder"
            } else {
                "no full-length Opus/AAC/MP3 stream within duration tolerance"
            }.into(),
        }))
    }

    /// Text search returning up to `limit` full-length, playable tracks.
    pub async fn search_tracks(&self, query: &str, limit: usize) -> Result<Vec<TrackMetadata>, ResolverError> {
        let query = query.trim();
        if query.is_empty() { return Err(ResolverError::InvalidInput("empty search query".into())); }
        if limit == 0 { return Ok(Vec::new()); }
        // Always ask for the 50-item maximum: the playable filter drops previews and blocks.
        let result = self.api(&format!("{API}/search/tracks"), &[("q", query), ("limit", "50")], SEARCH).await?;
        let items = result["collection"].as_array().ok_or_else(|| parse_error(SEARCH, "search collection missing"))?;
        let tracks = self.playable_tracks(items, None, &[], limit);
        if tracks.is_empty() { return Err(ResolverError::NotFound { service: SERVICE, query: query.into() }); }
        Ok(tracks)
    }

    /// Full-length, playable tracks related to a `soundcloud:track:<id>` seed.
    pub async fn related(&self, track_id: &str, limit: usize) -> Result<Vec<TrackMetadata>, ResolverError> {
        let (id, seed) = track_digits(track_id)?;
        if limit == 0 { return Ok(Vec::new()); }
        let mut last_error = None;
        // `/related` is a single page (no next_href; offset returns nothing).
        let mut tracks = match self.api(&format!("{API}/tracks/{id}/related"), &[("limit", "50")], RELATED).await {
            Ok(result) => match result["collection"].as_array() {
                Some(items) => self.playable_tracks(items, Some(seed), &[], limit),
                None => { last_error = Some(parse_error(RELATED, "related collection missing")); Vec::new() }
            },
            Err(error @ ResolverError::RateLimited { .. }) => return Err(error),
            Err(error) => { last_error = Some(error); Vec::new() }
        };
        // Major-label seeds get few related hits; the track station tops them up.
        if tracks.len() < RELATED_MIN.min(limit) {
            match self.station(id, seed, &tracks, limit - tracks.len()).await {
                Ok(more) => tracks.extend(more),
                Err(error @ ResolverError::RateLimited { .. }) => return Err(error),
                Err(error) => last_error = Some(error),
            }
        }
        if tracks.is_empty() {
            return Err(last_error.unwrap_or_else(|| ResolverError::NotFound { service: SERVICE, query: track_id.into() }));
        }
        Ok(tracks)
    }

    /// Station tracks for `id`, hydrated and filtered, skipping `seed` and `known`.
    async fn station(&self, id: &str, seed: u64, known: &[TrackMetadata], limit: usize)
        -> Result<Vec<TrackMetadata>, ResolverError> {
        let track = self.api(&format!("{API}/tracks/{id}"), &[], LOOKUP).await?;
        let urn = string(&track, "/station_urn").ok_or_else(|| parse_error(STATION, "station_urn missing"))?;
        if !valid_station_urn(&urn) { return Err(parse_error(STATION, "unexpected station_urn")); }
        let station = self.api(&format!("{API}/system-playlists/{urn}"), &[], STATION).await?;
        let stubs = station["tracks"].as_array().ok_or_else(|| parse_error(STATION, "station tracks missing"))?;
        let ids = station_ids(stubs, seed, known);
        if ids.is_empty() { return Ok(Vec::new()); }
        let joined = ids.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
        let hydrated = self.api(&format!("{API}/tracks"), &[("ids", &joined)], HYDRATE).await?;
        let items = hydrated.as_array().ok_or_else(|| parse_error(HYDRATE, "track array missing"))?;
        // `/tracks?ids=` does not promise station order; restore it.
        let ordered: Vec<Value> = ids.iter()
            .filter_map(|want| items.iter().find(|item| item["id"].as_u64() == Some(*want)).cloned()).collect();
        Ok(self.playable_tracks(&ordered, Some(seed), known, limit))
    }

    /// Parses up to `limit` playable tracks, dropping `exclude`, `known` and duplicates.
    fn playable_tracks(&self, items: &[Value], exclude: Option<u64>, known: &[TrackMetadata], limit: usize)
        -> Vec<TrackMetadata> {
        let mut seen = std::collections::HashSet::new();
        items.iter()
            .filter(|item| playable(item, self.native_only))
            .filter(|item| item["id"].as_u64().is_some_and(|id| Some(id) != exclude && seen.insert(id)))
            .filter_map(|item| parse_track(item).ok())
            .filter(|track| !known.iter().any(|k| k.id == track.id))
            .take(limit).collect()
    }

    async fn search(&self, query: &str) -> Result<Vec<Value>, ResolverError> {
        let result = self.api(&format!("{API}/search/tracks"), &[("q", query), ("limit", "25")], SEARCH).await?;
        result["collection"].as_array().cloned().ok_or_else(|| parse_error(SEARCH, "search collection missing"))
    }
}


impl MetadataProvider for SoundCloud {
    async fn track(&self, input: &str) -> Result<TrackMetadata, ResolverError> {
        let input = input.trim();
        if input.is_empty() { return Err(ResolverError::InvalidInput("empty search query".into())); }
        if input.contains("://") || input.starts_with("soundcloud:") {
            return parse_track(&self.lookup(input).await?);
        }
        let results = self.search(input).await?;
        results.first().ok_or_else(|| ResolverError::NotFound { service: SERVICE, query: input.into() })
            .and_then(parse_track)
    }

    async fn playlist(&self, input: &str) -> Result<Vec<TrackMetadata>, ResolverError> {
        let playlist = self.lookup(input).await?;
        if playlist["kind"].as_str() != Some("playlist") {
            return Err(ResolverError::InvalidInput("expected SoundCloud playlist URL".into()));
        }
        let items = playlist["tracks"].as_array().ok_or_else(|| parse_error(SERVICE, "playlist tracks missing"))?;
        if playlist["track_count"].as_u64() != Some(items.len() as u64) {
            return Err(ResolverError::Unavailable { service: SERVICE, message: "incomplete playlist".into() });
        }
        let mut tracks = Vec::with_capacity(items.len());
        for item in items {
            if item["title"].is_string() { tracks.push(parse_track(item)?); }
            else {
                let id = item["id"].as_u64().ok_or_else(|| parse_error(SERVICE, "track ID missing"))?;
                tracks.push(parse_track(&self.api(&format!("{API}/tracks/{id}"), &[], LOOKUP).await?)?);
            }
        }
        Ok(tracks)
    }
}

impl StreamResolver for SoundCloud {
    async fn resolve(&self, track: &TrackMetadata) -> Result<StreamInfo, ResolverError> {
        let search_queries = queries(track)?;
        let mut seen = std::collections::HashSet::new();
        let mut last_error = None;
        if track.id.starts_with("soundcloud:track:") {
            match self.lookup(&track.id).await {
                Ok(value) if value["duration"].as_u64().is_some_and(|d| duration_matches(track.duration_ms, d)) => {
                    match self.stream(&value, track.duration_ms).await {
                        Ok(stream) => return Ok(stream),
                        Err(error @ ResolverError::RateLimited { .. }) => return Err(error),
                        Err(error) => last_error = Some(error),
                    }
                }
                Err(error @ ResolverError::RateLimited { .. }) => return Err(error),
                Err(error) => last_error = Some(error),
                _ => {}
            }
        }
        for query in search_queries {
            let results = match self.search(&query).await {
                Ok(results) => results,
                Err(error @ ResolverError::RateLimited { .. }) => return Err(error),
                Err(error) => { last_error = Some(error); continue; }
            };
            for candidate in results {
                let Some(id) = candidate["id"].as_u64() else { continue };
                if !seen.insert(id) || !candidate["duration"].as_u64().is_some_and(|d| duration_matches(track.duration_ms, d)) {
                    continue;
                }
                // Fetch canonical track data: search entries may omit transcodings.
                let value = match self.api(&format!("{API}/tracks/{id}"), &[], LOOKUP).await {
                    Ok(value) => value,
                    Err(error @ ResolverError::RateLimited { .. }) => return Err(error),
                    Err(error) => { last_error = Some(error); continue; }
                };
                if !value["duration"].as_u64().is_some_and(|d| duration_matches(track.duration_ms, d)) { continue; }
                match self.stream(&value, track.duration_ms).await {
                    Ok(stream) => return Ok(stream),
                    Err(error @ ResolverError::RateLimited { .. }) => return Err(error),
                    Err(error) => last_error = Some(error),
                }
            }
        }
        Err(last_error.unwrap_or_else(|| ResolverError::NotFound {
            service: SERVICE, query: format!("{} (duration tolerance +/-3500 ms)", track.title),
        }))
    }
}

fn public_client_id(script: &str) -> Result<Option<&str>, ResolverError> {
    // Match complete quoted values, not prefixes or mismatched quote pairs.
    let pattern = Regex::new(
        r#"(?:^|[^\w$"'\\])(?:client_id|"client_id"|'client_id')\s*[:=]\s*(?:"(?P<double>[A-Za-z0-9]{32})"|'(?P<single>[A-Za-z0-9]{32})')"#,
    ).map_err(|e| parse_error(SCRIPT, e.to_string()))?;
    Ok(pattern.captures(script).and_then(|c| c.name("double").or_else(|| c.name("single")))
        .map(|id| id.as_str()))
}

fn public_script_urls(html: &str) -> Result<Vec<url::Url>, ResolverError> {
    // Consume quoted attributes whole so embedded '>' and data-src are not mistaken for src.
    let tags = Regex::new(r#"(?is)<!--.*?-->|<script(?P<attrs>(?:[\t\n\r\x0c ][^<>"']*|"[^"]*"|'[^']*')*)>"#)
        .map_err(|e| parse_error(HOMEPAGE, e.to_string()))?;
    let attributes = Regex::new(r#"[\t\n\r\x0c ]+([^\s=/'"<>]+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'=<>`]+)))?"#)
        .map_err(|e| parse_error(HOMEPAGE, e.to_string()))?;
    let mut urls = Vec::new();
    for tag in tags.captures_iter(html) {
        let Some(attrs) = tag.name("attrs") else { continue };
        for attr in attributes.captures_iter(attrs.as_str()) {
            if !attr[1].eq_ignore_ascii_case("src") { continue; }
            let Some(raw) = attr.get(2).or_else(|| attr.get(3)).or_else(|| attr.get(4)) else { break };
            let raw = raw.as_str().trim();
            let raw = if raw.starts_with("//") { format!("https:{raw}") } else { raw.to_owned() };
            if let Ok(url) = http::https_url(&raw, SCRIPT) {
                if url.host_str().is_some_and(|h| h.ends_with(".sndcdn.com"))
                    && url.username().is_empty() && url.password().is_none()
                    && url.port_or_known_default() == Some(443) {
                    urls.push(url);
                }
            }
            // HTML uses the first occurrence of a duplicate attribute.
            break;
        }
    }
    Ok(urls)
}

/// `soundcloud:track:<digits>` -> (digits, numeric ID); anything else is rejected
/// before it reaches a URL path.
fn track_digits(track_id: &str) -> Result<(&str, u64), ResolverError> {
    track_id.strip_prefix("soundcloud:track:")
        .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|id| Some((id, id.parse().ok()?)))
        .ok_or_else(|| ResolverError::InvalidInput("invalid SoundCloud track ID".into()))
}

/// The station URN goes into a URL path: accept only the observed shape.
fn valid_station_urn(urn: &str) -> bool {
    urn.strip_prefix("soundcloud:system-playlists:").is_some_and(|rest| !rest.is_empty())
        && urn.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'-' | b'_'))
}

/// Station stub IDs worth hydrating: at most 50 (one `/tracks?ids=` call), in order.
fn station_ids(stubs: &[Value], seed: u64, known: &[TrackMetadata]) -> Vec<u64> {
    let mut ids: Vec<u64> = Vec::new();
    for stub in stubs {
        let Some(id) = stub["id"].as_u64() else { continue };
        // Stubs carry policy: skip SNIP/BLOCK before spending a hydration slot.
        if stub["policy"].as_str().is_some_and(|p| p != "ALLOW") || id == seed || ids.contains(&id)
            || known.iter().any(|k| k.id == format!("soundcloud:track:{id}")) { continue; }
        ids.push(id);
        if ids.len() == 50 { break; }
    }
    ids
}

fn restricted(value: &Value) -> bool {
    value["policy"].as_str().is_some_and(|p| p != "ALLOW") || value["streamable"].as_bool() != Some(true)
}

/// Codec and protocol of a transcoding `stream()` can play, ignoring duration.
fn transcoding_format(t: &Value, native_only: bool) -> Option<(AudioCodec, &str)> {
    if t["snipped"].as_bool() == Some(true) { return None; }
    // Encrypted variants ("cbc-encrypted-hls", ...) need DRM and fall out here.
    let protocol = t.pointer("/format/protocol")?.as_str()?;
    if !matches!(protocol, "hls" | "progressive") { return None; }
    let mime = t.pointer("/format/mime_type")?.as_str()?;
    let codec = if mime.contains("opus") { AudioCodec::Opus }
        else if mime == "audio/mp4; codecs=\"mp4a.40.2\"" { AudioCodec::Aac }
        // "audio/mpegurl" is an adaptive variant (observed HTTP 404), not MP3.
        else if mime == "audio/mpeg" { AudioCodec::Mp3 } else { return None; };
    // Most uploads now offer only HLS AAC (fMP4); the native decoder reads
    // those sequentially. Opus still needs FFmpeg.
    if native_only && codec == AudioCodec::Opus { return None; }
    Some((codec, protocol))
}

/// True when `stream()` would find a full-length candidate for this track, so
/// recommendation queues are not filled with previews, geo-blocks or DRM-only uploads.
fn playable(value: &Value, native_only: bool) -> bool {
    if restricted(value) { return false; }
    let Some(duration) = value["duration"].as_u64().filter(|d| *d > 0) else { return false };
    // SNIP previews report duration 30000 against the real full_duration.
    if value["full_duration"].as_u64().is_some_and(|full| !duration_matches(full, duration)) { return false; }
    value.pointer("/media/transcodings").and_then(Value::as_array).is_some_and(|ts| ts.iter().any(|t|
        t["duration"].as_u64().is_some_and(|d| duration_matches(duration, d))
            && transcoding_format(t, native_only).is_some()))
}

fn parse_track(value: &Value) -> Result<TrackMetadata, ResolverError> {
    if value["kind"].as_str() != Some("track") { return Err(parse_error(SERVICE, "not a track")); }
    let id = value["id"].as_u64().ok_or_else(|| parse_error(SERVICE, "track ID missing"))?;
    let duration_ms = value["duration"].as_u64().filter(|d| *d > 0)
        .ok_or_else(|| parse_error(SERVICE, "duration missing"))?;
    let artist = string(value, "/publisher_metadata/artist").or_else(|| string(value, "/user/username"))
        .ok_or_else(|| parse_error(SERVICE, "artist/uploader missing"))?;
    Ok(TrackMetadata {
        id: format!("soundcloud:track:{id}"), title: required_string(value, "/title", SERVICE)?,
        artists: vec![artist], duration_ms,
        album: string(value, "/publisher_metadata/album_title"),
        cover_url: string(value, "/artwork_url").or_else(|| string(value, "/user/avatar_url")),
        isrc: string(value, "/publisher_metadata/isrc"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn public_client_id_formats() {
        let id = "a".repeat(32);
        for key in ["client_id", "\"client_id\"", "'client_id'"] {
            for quote in ['\'', '"'] {
                for separator in [":", " = ", "\n:\t"] {
                    let script = format!("{{ {key}{separator}{quote}{id}{quote} }}");
                    assert!(public_client_id(&script).unwrap() == Some(id.as_str()));
                }
            }
        }
    }

    #[test]
    fn malformed_client_ids_are_skipped() {
        let id = "a".repeat(32);
        for candidate in [
            format!("client_id:'{id}extra'"),
            format!("client_id:'{}'", "a".repeat(31)),
            format!("client_id:'{}!'", "a".repeat(31)),
            format!("client_id:'{id}\""),
            format!("client_id:\"{id}'"),
            format!("client_id:'{id}"),
            format!("client_id:{id}"),
            format!("other_client_id:'{id}'"),
            format!("$client_id:'{id}'"),
            format!("'other_client_id':'{id}'"),
            format!("'client_id\":'{id}'"),
            format!("client_id == '{id}'"),
        ] {
            assert!(public_client_id(&candidate).unwrap().is_none());
            let script = format!("{candidate};\n{{\"client_id\" : \"{id}\"}}");
            assert!(public_client_id(&script).unwrap() == Some(id.as_str()));
        }
    }

    #[test]
    fn public_script_tag_formats() {
        let html = r#"
            <script src="https://a-v2.sndcdn.com/one.js"></script>
            <SCRIPT defer SRC = '//a-v2.sndcdn.com/two.js'></SCRIPT>
            <script data-note="a > b src='https://evil.invalid/fake.js'"
                src = "https://a-v2.sndcdn.com/three.js"></script>
            <script src=https://a-v2.sndcdn.com/four.js></script>
        "#;
        let urls = public_script_urls(html).unwrap();
        assert_eq!(urls.iter().map(|u| u.path()).collect::<Vec<_>>(),
            ["/one.js", "/two.js", "/three.js", "/four.js"]);
        assert!(urls.iter().all(|u| u.scheme() == "https"));
    }

    #[test]
    fn unsafe_or_malformed_script_tags_are_skipped() {
        for html in [
            r#"<script data-src="https://a.sndcdn.com/no.js">"#,
            r#"<scripture src="https://a.sndcdn.com/no.js">"#,
            r#"<!-- <script src="https://a.sndcdn.com/no.js"> -->"#,
            r#"<script src='https://a.sndcdn.com/no.js">"#,
            r#"<script src="http://a.sndcdn.com/no.js">"#,
            r#"<script src="https://sndcdn.com.evil.invalid/no.js">"#,
            r#"<script src="https://evilsndcdn.com/no.js">"#,
            r#"<script src="https://evil.invalid/a.sndcdn.com/no.js">"#,
            r#"<script src="https://user@a.sndcdn.com/no.js">"#,
            r#"<script src="https://a.sndcdn.com:444/no.js">"#,
            r#"<script src="/no.js">"#,
            r#"<script src="https://evil.invalid/no.js" src="https://a.sndcdn.com/no.js">"#,
        ] {
            assert!(public_script_urls(html).unwrap().is_empty());
            let html = format!("{html}\n<script src='https://a.sndcdn.com/ok.js'></script>");
            let urls = public_script_urls(&html).unwrap();
            assert_eq!(urls.len(), 1);
            assert_eq!(urls[0].path(), "/ok.js");
        }
    }

    #[tokio::test]
    async fn invalid_transcoding_host_does_not_expose_secrets() {
        let resolver = SoundCloud::new().unwrap();
        let error = resolver.api("https://example.invalid/private?token=SECRET", &[], TRANSCODING)
            .await.unwrap_err();
        assert!(matches!(error, ResolverError::Parse { service: TRANSCODING, .. }));
        assert_eq!(error.to_string(), "SoundCloud [transcoding lookup]: invalid response: unexpected API host");
        assert!(!format!("{error:?}").contains("SECRET"));
    }

    #[tokio::test]
    async fn restricted_tracks_are_not_requested() {
        let resolver = SoundCloud::new().unwrap();
        for value in [json!({"policy": "BLOCK", "streamable": true}),
            json!({"policy": "ALLOW", "streamable": false})] {
            let error = resolver.stream(&value, 10000).await.unwrap_err();
            assert!(matches!(error, ResolverError::Unavailable { .. }));
            assert!(error.to_string().contains("restricted or not streamable"));
        }
    }

    #[tokio::test]
    async fn native_mode_rejects_opus_and_adaptive_variants() {
        let resolver = SoundCloud::new_native().unwrap();
        for mime in ["audio/ogg; codecs=\"opus\"", "audio/mpegurl"] {
            let value = json!({"policy": "ALLOW", "streamable": true, "media": {"transcodings": [{
                "duration": 10000, "format": {"protocol": "hls", "mime_type": mime},
                "url": "https://example.invalid/private?token=SECRET"
            }]}});
            let error = resolver.stream(&value, 10000).await.unwrap_err();
            assert!(matches!(error, ResolverError::Unavailable { .. }));
            assert!(error.to_string().contains("Opus is not supported"));
        }
    }

    #[tokio::test]
    async fn native_mode_selects_hls_aac_and_mp3() {
        let resolver = SoundCloud::new_native().unwrap();
        for mime in ["audio/mp4; codecs=\"mp4a.40.2\"", "audio/mpeg"] {
            let value = json!({"policy": "ALLOW", "streamable": true, "media": {"transcodings": [{
                "duration": 10000, "format": {"protocol": "hls", "mime_type": mime},
                "url": "https://example.invalid/never-requested?token=SECRET"
            }]}});
            // Host validation proves selection without making a network request.
            assert!(matches!(resolver.stream(&value, 10000).await.unwrap_err(),
                ResolverError::Parse { service: TRANSCODING, .. }));
        }
    }

    #[tokio::test]
    async fn aac_selection_preserves_native_and_track_guards() {
        let resolver = SoundCloud::new().unwrap();
        let native = SoundCloud::new_native().unwrap();
        let value = json!({"policy": "ALLOW", "streamable": true, "media": {"transcodings": [{
            "duration": 10000, "snipped": false,
            "format": {"protocol": "hls", "mime_type": "audio/mp4; codecs=\"mp4a.40.2\""},
            "url": "https://example.invalid/never-requested?token=SECRET"
        }]}});
        // Host validation proves selection without making a network request.
        let error = resolver.stream(&value, 10000).await.unwrap_err();
        assert!(matches!(error, ResolverError::Parse { service: TRANSCODING, .. }));
        for protocol in ["hls", "progressive"] {
            let mut candidate = value.clone();
            candidate["media"]["transcodings"][0]["format"]["protocol"] = json!(protocol);
            assert!(matches!(native.stream(&candidate, 10000).await.unwrap_err(),
                ResolverError::Parse { service: TRANSCODING, .. }));
        }
        for (pointer, replacement) in [
            ("/policy", json!("BLOCK")),
            ("/streamable", json!(false)),
            ("/media/transcodings/0/snipped", json!(true)),
            ("/media/transcodings/0/duration", json!(13501)),
            ("/media/transcodings/0/format/protocol", json!("unknown")),
            ("/media/transcodings/0/format/mime_type", json!("audio/mp4")),
        ] {
            let mut candidate = value.clone();
            *candidate.pointer_mut(pointer).unwrap() = replacement;
            assert!(matches!(resolver.stream(&candidate, 10000).await.unwrap_err(),
                ResolverError::Unavailable { .. }));
        }
        for duration in [6500, 13500] {
            let mut candidate = value.clone();
            candidate["media"]["transcodings"][0]["duration"] = json!(duration);
            assert!(matches!(resolver.stream(&candidate, 10000).await.unwrap_err(),
                ResolverError::Parse { service: TRANSCODING, .. }));
        }
    }

    #[tokio::test]
    async fn stage_labels_preserve_http_and_rate_limit_fields() {
        use std::io::{Read, Write};
        for (status, retry) in [(404, None), (429, Some(123))] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
                let mut request = [0; 4096];
                socket.read(&mut request).unwrap();
                write!(socket, "HTTP/1.1 {status} Test\r\nRetry-After: 123\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            });
            let client = reqwest::Client::builder().no_proxy()
                .timeout(std::time::Duration::from_secs(5)).build().unwrap();
            let error = http::response(client.get(format!("http://{address}/?token=SECRET")), TRANSCODING)
                .await.unwrap_err();
            server.join().unwrap();
            match (error, retry) {
                (ResolverError::Http { service, status: actual }, None) => {
                    assert_eq!(service, TRANSCODING);
                    assert_eq!(actual, status);
                }
                (ResolverError::RateLimited { service, retry_after_secs }, Some(expected)) => {
                    assert_eq!(service, TRANSCODING);
                    assert_eq!(retry_after_secs, expected);
                }
                (error, _) => panic!("unexpected error: {error}"),
            }
        }
    }


    fn playable_fixture() -> Value {
        json!({"kind": "track", "id": 7, "title": "Song", "duration": 200000, "full_duration": 200000,
            "policy": "ALLOW", "streamable": true, "user": {"username": "Artist"},
            "media": {"transcodings": [{"duration": 200000, "snipped": false,
                "format": {"protocol": "hls", "mime_type": "audio/mp4; codecs=\"mp4a.40.2\""},
                "url": "https://api-v2.soundcloud.com/media/x"}]}})
    }

    #[test]
    fn playable_filter_matches_stream_selection() {
        let value = playable_fixture();
        assert!(playable(&value, false) && playable(&value, true));
        let mut absent = value.clone();
        absent.as_object_mut().unwrap().remove("policy");
        assert!(playable(&absent, true));
        for (pointer, replacement) in [
            ("/policy", json!("SNIP")),
            ("/policy", json!("BLOCK")),
            ("/streamable", json!(false)),
            ("/duration", json!(0)),
            // SNIP previews: 30 s duration against the full length.
            ("/duration", json!(30000)),
            ("/media/transcodings/0/snipped", json!(true)),
            ("/media/transcodings/0/duration", json!(30000)),
            ("/media/transcodings/0/format/protocol", json!("cbc-encrypted-hls")),
            ("/media/transcodings/0/format/protocol", json!("ctr-encrypted-hls")),
            ("/media/transcodings/0/format/mime_type", json!("audio/mpegurl")),
            ("/media/transcodings", json!([])),
        ] {
            let mut candidate = value.clone();
            *candidate.pointer_mut(pointer).unwrap() = replacement;
            assert!(!playable(&candidate, false), "{pointer}");
        }
        let mut missing = value.clone();
        missing.as_object_mut().unwrap().remove("media");
        assert!(!playable(&missing, false));
    }

    #[test]
    fn playable_filter_respects_native_opus_and_any_usable_variant() {
        let mut opus = playable_fixture();
        opus["media"]["transcodings"][0]["format"]["mime_type"] = json!("audio/ogg; codecs=\"opus\"");
        assert!(playable(&opus, false));
        assert!(!playable(&opus, true));
        // One encrypted and one usable MP3 variant: the usable one wins.
        let mut mixed = playable_fixture();
        mixed["media"]["transcodings"] = json!([
            {"duration": 200000, "format": {"protocol": "ctr-encrypted-hls", "mime_type": "audio/mp4; codecs=\"mp4a.40.2\""}},
            {"duration": 200000, "format": {"protocol": "progressive", "mime_type": "audio/mpeg"}},
        ]);
        assert!(playable(&mixed, true));
    }

    #[test]
    fn playable_tracks_drop_seed_duplicates_and_known() {
        let resolver = SoundCloud::new_native().unwrap();
        let mut items = Vec::new();
        for id in [1, 2, 2, 3, 4, 5] {
            let mut item = playable_fixture();
            item["id"] = json!(id);
            items.push(item);
        }
        items[4]["policy"] = json!("BLOCK");
        let known = parse_track(&items[3]).unwrap();
        let tracks = resolver.playable_tracks(&items, Some(1), &[known], 10);
        assert_eq!(tracks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["soundcloud:track:2", "soundcloud:track:5"]);
        assert_eq!(resolver.playable_tracks(&items, None, &[], 1).len(), 1);
    }

    #[test]
    fn station_stubs_skip_restricted_seed_and_cap_at_fifty() {
        let mut stubs: Vec<Value> = (1..=60).map(|id| json!({"id": id, "kind": "track", "policy": "ALLOW"})).collect();
        stubs[1]["policy"] = json!("SNIP");
        stubs.push(json!({"id": 3, "policy": "ALLOW"}));
        stubs.insert(0, json!({"kind": "track"}));
        let known = parse_track(&playable_fixture()).unwrap();
        let ids = station_ids(&stubs, 1, &[known]);
        assert_eq!(ids.len(), 50);
        assert_eq!(&ids[..3], &[3, 4, 5]);
        assert!(!ids.contains(&7) && !ids.contains(&2));
    }

    #[tokio::test]
    async fn related_rejects_invalid_ids_before_requests() {
        let resolver = SoundCloud::new().unwrap();
        assert_eq!(track_digits("soundcloud:track:123").unwrap(), ("123", 123));
        for id in ["", "123", "soundcloud:track:", "soundcloud:track:12a", "soundcloud:track:1/related",
            "soundcloud:playlist:1", "youtube:video:dQw4w9WgXcQ", "soundcloud:track:99999999999999999999999"] {
            assert!(matches!(resolver.related(id, 10).await.unwrap_err(), ResolverError::InvalidInput(_)), "{id}");
        }
        assert!(matches!(resolver.search_tracks("  ", 10).await.unwrap_err(), ResolverError::InvalidInput(_)));
    }

    #[test]
    fn station_urn_shape() {
        assert!(valid_station_urn("soundcloud:system-playlists:track-stations:2336237564"));
        for urn in ["", "soundcloud:system-playlists:", "soundcloud:tracks:1",
            "soundcloud:system-playlists:x/../../me", "soundcloud:system-playlists:x?client_id=y"] {
            assert!(!valid_station_urn(urn), "{urn}");
        }
    }

    /// Opt-in network diagnostic: only public metadata and availability probes.
    #[tokio::test]
    #[ignore = "requires public SoundCloud network access"]
    async fn public_track_availability() {
        let resolver = SoundCloud::new().unwrap();
        let value = resolver.lookup("https://soundcloud.com/forss/flickermood").await.unwrap();
        let track = parse_track(&value).unwrap();
        println!("Public URL metadata: OK");
        let value = resolver.lookup(&track.id).await.unwrap();
        println!("Canonical track lookup: OK");
        let formats = value.pointer("/media/transcodings").and_then(Value::as_array).unwrap();
        println!("Advertised transcoding count: {}", formats.len());
        for (index, format) in formats.iter().enumerate() {
            let protocol = match format.pointer("/format/protocol").and_then(Value::as_str) {
                Some("hls") => "hls",
                Some("progressive") => "progressive",
                _ => "other",
            };
            let mime = match format.pointer("/format/mime_type").and_then(Value::as_str) {
                Some("audio/mpeg") => "MP3",
                Some("audio/ogg; codecs=\"opus\"") => "Opus",
                Some("audio/mp4") => "MP4 audio",
                Some("audio/mp4; codecs=\"mp4a.40.2\"") => "audio/mp4; codecs=mp4a.40.2",
                Some("audio/mp4; codecs=\"mp4a.40.5\"") => "audio/mp4; codecs=mp4a.40.5",
                Some("audio/aac") => "audio/aac",
                Some("audio/ogg; codecs=opus") => "audio/ogg; codecs=opus",
                _ => "other",
            };
            println!("Format {index}: protocol={protocol}, type={mime}, duration_ms={:?}, snipped={:?}",
                format["duration"].as_u64(), format["snipped"].as_bool());
            let mut single = value.clone();
            single["media"]["transcodings"] = json!([format]);
            match resolver.stream(&single, track.duration_ms).await {
                Ok(stream) => println!("Format {index}: available ({:?}); no playback tested", stream.codec),
                Err(error) => {
                    println!("Format {index}: {error}");
                    if matches!(error, ResolverError::RateLimited { .. }) { break; }
                }
            }
        }
    }
}
