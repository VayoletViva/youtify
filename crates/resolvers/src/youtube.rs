use std::{cmp::Reverse, collections::HashSet};
use core_types::{AudioCodec, ResolverError, StreamInfo, StreamResolver, StreamSource, TrackMetadata};
use rustypipe::{client::RustyPipe, model::{AudioCodec as YtCodec, TrackItem, YouTubeItem}};
use crate::{duration_matches, expires_at, http, queries};

const SERVICE: &str = "YouTube";

pub struct YouTube {
    pipe: RustyPipe,
    client: reqwest::Client,
    native_only: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::TcpListener, time::Duration};

    #[tokio::test]
    async fn visitor_data_transport_failure_returns_error_without_panicking() {
        // A local proxy that never responds makes this independent of DNS/YouTube.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(format!("http://{}", listener.local_addr().unwrap())).unwrap());
        let pipe = RustyPipe::builder().no_botguard().no_storage().no_reporter()
            .unauthenticated().timeout(Duration::from_millis(100)).build_with_client(client).unwrap();
        // Failed initialization must also leave the cache usable for another attempt.
        for _ in 0..2 {
            let error = pipe.query().get_visitor_data(false).await.unwrap_err();
            assert!(matches!(error, rustypipe::error::Error::Http(_)));
            assert!(matches!(pipe_error(error), ResolverError::Unavailable { service: SERVICE, .. }));
        }
    }

    /// `TrackItem` is `#[non_exhaustive]`; build it through its serde form.
    fn item(id: &str, track_type: &str, duration: Option<u32>, artists: &[&str]) -> TrackItem {
        serde_json::from_value(serde_json::json!({
            "id": id, "name": format!("Song {id}"), "duration": duration,
            "cover": [
                {"url": "https://lh3.googleusercontent.com/a=w60-h60", "width": 60, "height": 60},
                {"url": "https://lh3.googleusercontent.com/a=w544-h544", "width": 544, "height": 544},
                {"url": "https://lh3.googleusercontent.com/a=w120-h120", "width": 120, "height": 120},
            ],
            "artists": artists.iter().map(|name| serde_json::json!({"id": null, "name": name})).collect::<Vec<_>>(),
            "artist_id": null, "album": {"id": "MPREb_x", "name": "Album"}, "view_count": null,
            "track_type": track_type, "track_nr": null, "by_va": false,
        })).unwrap()
    }

    #[test]
    fn video_id_accepts_only_youtube_video_ids() {
        assert_eq!(video_id("youtube:video:dQw4w9WgXcQ"), Some("dQw4w9WgXcQ"));
        assert_eq!(video_id("youtube:video:a-b_c123456"), Some("a-b_c123456"));
        for id in ["dQw4w9WgXcQ", "youtube:video:", "youtube:video:short", "youtube:video:dQw4w9WgXcQx",
            "youtube:video:dQw4w9WgX/Q", "soundcloud:track:12345678901", "deezer:track:3135556"] {
            assert_eq!(video_id(id), None, "{id}");
        }
    }

    #[test]
    fn track_item_maps_to_metadata() {
        let track = track_metadata(&item("hh5GKVa8VtM", "track", Some(178), &["IVE", " ", "Other"])).unwrap();
        assert_eq!(track.id, "youtube:video:hh5GKVa8VtM");
        assert_eq!(track.title, "Song hh5GKVa8VtM");
        assert_eq!(track.artists, ["IVE", "Other"]);
        assert_eq!(track.duration_ms, 178_000);
        assert_eq!(track.album.as_deref(), Some("Album"));
        assert_eq!(track.cover_url.as_deref(), Some("https://lh3.googleusercontent.com/a=w544-h544"));
        assert_eq!(track.isrc, None);
        assert!(track_metadata(&item("hh5GKVa8VtM", "track", None, &["IVE"])).is_none());
        assert!(track_metadata(&item("hh5GKVa8VtM", "track", Some(0), &["IVE"])).is_none());
        assert!(track_metadata(&item("hh5GKVa8VtM", "track", Some(178), &[])).is_none());
        assert!(track_metadata(&item("bad/id", "track", Some(178), &["IVE"])).is_none());
    }

    #[test]
    fn radio_prefers_tracks_and_excludes_seed() {
        let items = [
            item("seedSEED123", "track", Some(200), &["A"]),
            item("video000001", "video", Some(200), &["B"]),
            item("track000001", "track", Some(200), &["C"]),
            item("track000001", "track", Some(200), &["C"]),
            item("track000002", "track", None, &["D"]),
            item("track000003", "track", Some(200), &["E"]),
        ];
        let ids = |tracks: Vec<TrackMetadata>| tracks.into_iter().map(|t| t.id).collect::<Vec<_>>();
        assert_eq!(ids(radio_tracks(&items, "seedSEED123", 2)),
            ["youtube:video:track000001", "youtube:video:track000003"]);
        // Too few official tracks: music videos fill in, in radio order.
        assert_eq!(ids(radio_tracks(&items, "seedSEED123", 5)),
            ["youtube:video:video000001", "youtube:video:track000001", "youtube:video:track000003"]);
    }

    #[test]
    fn seed_search_prefers_duration_match() {
        let items = [item("first000001", "track", Some(100), &["A"]), item("match000001", "track", Some(204), &["A"])];
        assert_eq!(seed_video(&items, 200_000).unwrap().id, "match000001");
        assert_eq!(seed_video(&items, 500_000).unwrap().id, "first000001");
        assert!(seed_video(&[], 200_000).is_none());
    }
}

impl YouTube {
    pub fn new() -> Result<Self, ResolverError> {
        let pipe = RustyPipe::builder().no_botguard().no_storage().no_reporter()
            .unauthenticated().build().map_err(pipe_error)?;
        Ok(Self { pipe, client: http::client()?, native_only: false })
    }

    /// AAC only, for the built-in Symphonia decoder (including Android).
    pub fn new_native() -> Result<Self, ResolverError> {
        let mut resolver = Self::new()?;
        resolver.native_only = true;
        Ok(resolver)
    }
}

impl YouTube {
    /// YouTube Music radio seeded by `seed`: up to `limit` tracks with
    /// `youtube:video:<id>` IDs, excluding the seed itself.
    pub async fn radio(&self, seed: &TrackMetadata, limit: usize) -> Result<Vec<TrackMetadata>, ResolverError> {
        if limit == 0 { return Ok(Vec::new()); }
        let query = self.pipe.query();
        let seed_id = match video_id(&seed.id) {
            Some(id) => id.to_owned(),
            None => {
                let title = seed.title.trim();
                if title.is_empty() { return Err(ResolverError::InvalidInput("seed title is required".into())); }
                let text = if seed.artists.is_empty() { title.to_owned() }
                    else { format!("{}, {title}", seed.artists.join(", ")) };
                let found = query.music_search_tracks(&text).await.map_err(pipe_error)?;
                seed_video(&found.items.items, seed.duration_ms).map(|t| t.id.clone())
                    .ok_or_else(|| ResolverError::NotFound { service: SERVICE, query: title.into() })?
            }
        };
        let mut radio = query.music_radio_track(&seed_id).await.map_err(pipe_error)?;
        // Headroom for the seed, music videos and items without duration.
        radio.extend_limit(&query, limit + 5).await.map_err(pipe_error)?;
        let tracks = radio_tracks(&radio.items, &seed_id, limit);
        if tracks.is_empty() {
            return Err(ResolverError::NotFound { service: SERVICE, query: format!("radio {seed_id}") });
        }
        Ok(tracks)
    }
}

fn pipe_error(error: rustypipe::error::Error) -> ResolverError {
    ResolverError::Unavailable { service: SERVICE, message: error.to_string() }
}

impl StreamResolver for YouTube {
    async fn resolve(&self, track: &TrackMetadata) -> Result<StreamInfo, ResolverError> {
        let search_queries = queries(track)?;
        let mut seen = HashSet::new();
        let mut last_error = None;
        // Radio/library entries already name their video: try it before searching.
        if let Some(id) = video_id(&track.id) {
            seen.insert(id.to_owned());
            if let Some(stream) = self.video_stream(id, track.duration_ms, &mut last_error).await {
                return Ok(stream);
            }
        }
        for query in search_queries {
            let search = match self.pipe.query().search::<YouTubeItem, _>(&query).await {
                Ok(result) => result,
                Err(error) => { last_error = Some(pipe_error(error)); continue; }
            };
            for item in search.items.items.into_iter().take(25) {
                let YouTubeItem::Video(video) = item else { continue };
                if video.is_live || video.is_upcoming || !seen.insert(video.id.clone()) { continue; }
                // Search durations have only second precision. Final acceptance below
                // always requires the audio stream's exact millisecond duration.
                if let Some(seconds) = video.duration {
                    if (u64::from(seconds) * 1000).abs_diff(track.duration_ms) > 4_499 { continue; }
                }
                if let Some(stream) = self.video_stream(&video.id, track.duration_ms, &mut last_error).await {
                    return Ok(stream);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| ResolverError::NotFound {
            service: SERVICE, query: format!("{} (duration tolerance +/-3500 ms)", track.title),
        }))
    }
}

impl YouTube {
    /// First probed, DRM-free audio stream of `id` whose exact duration matches.
    /// Failures land in `last_error` so callers can keep trying candidates.
    async fn video_stream(&self, id: &str, expected_ms: u64, last_error: &mut Option<ResolverError>)
        -> Option<StreamInfo> {
        let player = match self.pipe.query().player(id).await {
            Ok(player) => player,
            Err(error) => { *last_error = Some(pipe_error(error)); return None; }
        };
        if player.drm.is_some() { return None; }
        let mut streams: Vec<_> = player.audio_streams.iter().filter(|stream| {
            stream.drm_systems.is_empty() && stream.drm_track_type.is_none()
                && (!self.native_only || stream.codec == YtCodec::Mp4a)
                && stream.duration_ms.is_some_and(|d| duration_matches(expected_ms, u64::from(d)))
                && matches!(stream.codec, YtCodec::Opus | YtCodec::Mp4a)
        }).collect();
        streams.sort_by_key(|s| (
            if s.itag == 251 { 0 } else if s.codec == YtCodec::Opus { 1 } else { 2 },
            Reverse(s.bitrate),
        ));
        for stream in streams {
            if http::https_url(&stream.url, SERVICE).is_err() { continue; }
            // A URL alone is not proof of playback: PO-token restrictions can
            // cause 403 responses. Probe only headers and drop the body.
            match http::probe_media(&self.client, &stream.url, SERVICE, self.native_only).await {
                Ok(_) => return Some(StreamInfo {
                    stream_url: stream.url.clone(), hls: false,
                    codec: if stream.codec == YtCodec::Opus { AudioCodec::Opus } else { AudioCodec::Aac },
                    bitrate: Some(stream.bitrate), source: StreamSource::YouTube,
                    expires_at: match (expires_at(&stream.url), u64::try_from(player.valid_until.unix_timestamp()).ok()) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (a, b) => a.or(b),
                    },
                }),
                Err(error) => *last_error = Some(error),
            }
        }
        None
    }
}

/// `youtube:video:<id>` -> id, only for the 11-character URL-safe video ID shape.
fn video_id(track_id: &str) -> Option<&str> {
    track_id.strip_prefix("youtube:video:").filter(|id| id.len() == 11
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')))
}

/// Radio/search item -> metadata. `None` without duration, artists or a valid ID:
/// the stream resolver needs all three.
fn track_metadata(item: &TrackItem) -> Option<TrackMetadata> {
    let id = format!("youtube:video:{}", item.id);
    video_id(&id)?;
    let duration_ms = u64::from(item.duration.filter(|d| *d > 0)?) * 1000;
    let artists: Vec<String> = item.artists.iter().map(|a| a.name.trim())
        .filter(|name| !name.is_empty()).map(str::to_owned).collect();
    let title = item.name.trim();
    if artists.is_empty() || title.is_empty() { return None; }
    Some(TrackMetadata {
        id, title: title.to_owned(), artists, duration_ms,
        album: item.album.as_ref().map(|a| a.name.trim()).filter(|n| !n.is_empty()).map(str::to_owned),
        cover_url: item.cover.iter().max_by_key(|t| u64::from(t.width) * u64::from(t.height)).map(|t| t.url.clone()),
        isrc: None,
    })
}

/// Search hit for the seed: the first within +/-5 s of its duration, else the first.
fn seed_video(items: &[TrackItem], duration_ms: u64) -> Option<&TrackItem> {
    items.iter().find(|t| t.duration.is_some_and(|d| (u64::from(d) * 1000).abs_diff(duration_ms) <= 5_000))
        .or_else(|| items.first())
}

/// Up to `limit` radio entries without the seed or duplicates. Official audio
/// tracks are preferred; music videos are kept only when tracks alone fall short.
fn radio_tracks(items: &[TrackItem], seed: &str, limit: usize) -> Vec<TrackMetadata> {
    let mut seen = HashSet::new();
    let all: Vec<(bool, TrackMetadata)> = items.iter().filter(|t| t.id != seed && seen.insert(t.id.as_str()))
        .filter_map(|t| Some((t.track_type.is_track(), track_metadata(t)?))).collect();
    let only_tracks = all.iter().filter(|(track, _)| *track).count() >= limit;
    all.into_iter().filter(|(track, _)| *track || !only_tracks).map(|(_, m)| m).take(limit).collect()
}
