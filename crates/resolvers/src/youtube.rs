use std::{cmp::Reverse, collections::HashSet};
use core_types::{AudioCodec, ResolverError, StreamInfo, StreamResolver, StreamSource, TrackMetadata};
use rustypipe::{client::RustyPipe, model::{AudioCodec as YtCodec, YouTubeItem}};
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

fn pipe_error(error: rustypipe::error::Error) -> ResolverError {
    ResolverError::Unavailable { service: SERVICE, message: error.to_string() }
}

impl StreamResolver for YouTube {
    async fn resolve(&self, track: &TrackMetadata) -> Result<StreamInfo, ResolverError> {
        let mut seen = HashSet::new();
        let mut last_error = None;
        for query in queries(track)? {
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
                let player = match self.pipe.query().player(&video.id).await {
                    Ok(player) => player,
                    Err(error) => { last_error = Some(pipe_error(error)); continue; }
                };
                if player.drm.is_some() { continue; }
                let mut streams: Vec<_> = player.audio_streams.iter().filter(|stream| {
                    stream.drm_systems.is_empty() && stream.drm_track_type.is_none()
                        && (!self.native_only || stream.codec == YtCodec::Mp4a)
                        && stream.duration_ms.is_some_and(|d| duration_matches(track.duration_ms, u64::from(d)))
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
                        Ok(_) => return Ok(StreamInfo {
                            stream_url: stream.url.clone(),
                            codec: if stream.codec == YtCodec::Opus { AudioCodec::Opus } else { AudioCodec::Aac },
                            bitrate: Some(stream.bitrate), source: StreamSource::YouTube,
                            expires_at: match (expires_at(&stream.url), u64::try_from(player.valid_until.unix_timestamp()).ok()) {
                                (Some(a), Some(b)) => Some(a.min(b)),
                                (a, b) => a.or(b),
                            },
                        }),
                        Err(error) => last_error = Some(error),
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| ResolverError::NotFound {
            service: SERVICE, query: format!("{} (duration tolerance +/-3500 ms)", track.title),
        }))
    }
}
