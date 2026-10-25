//! Sequential HLS VOD reader for the native decoder. The optional init map and
//! media segments are concatenated into one forward-only byte stream; at most
//! the current and one prefetched segment are held in memory. Encrypted,
//! byte-range, live and master playlists are rejected rather than misplayed.
use reqwest::Client;
use std::{collections::VecDeque, io::{self, Read, Seek, SeekFrom}, sync::{Arc, atomic::{AtomicBool, Ordering}}, time::Duration};
use tokio::{runtime::Handle, task::JoinHandle};
use url::Url;

const MAX_PLAYLIST: usize = 4 * 1024 * 1024;
const MAX_SEGMENT: usize = 32 * 1024 * 1024;

fn error(message: impl ToString) -> io::Error { io::Error::other(message.to_string()) }

#[derive(Debug, PartialEq)]
pub struct Playlist {
    pub init: Option<String>,
    /// Absolute segment URL and its duration in seconds.
    pub segments: Vec<(String, f64)>,
}

fn quoted_attribute<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let start = line.find(&format!("{name}=\""))? + name.len() + 2;
    line[start..].split('"').next()
}

pub fn parse(base: &Url, text: &str) -> io::Result<Playlist> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    if lines.next() != Some("#EXTM3U") { return Err(error("HLS: not an M3U playlist")); }
    let resolve = |raw: &str| base.join(raw).ok().filter(|u| u.scheme() == "https").map(String::from)
        .ok_or_else(|| error("HLS: invalid segment URL"));
    let (mut init, mut segments, mut duration, mut vod) = (None, Vec::new(), None, false);
    for line in lines {
        if let Some(value) = line.strip_prefix("#EXTINF:") {
            let seconds = value.split(',').next().and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|v| v.is_finite() && *v >= 0.0).ok_or_else(|| error("HLS: invalid EXTINF"))?;
            duration = Some(seconds);
        } else if line.starts_with("#EXT-X-MAP:") {
            if init.is_some() { return Err(error("HLS: multiple init maps are not supported")); }
            if line.contains("BYTERANGE=") { return Err(error("HLS: byte-range maps are not supported")); }
            init = Some(resolve(quoted_attribute(line, "URI").ok_or_else(|| error("HLS: map without URI"))?)?);
        } else if line.starts_with("#EXT-X-STREAM-INF") {
            return Err(error("HLS: master playlists are not supported"));
        } else if line.starts_with("#EXT-X-BYTERANGE") {
            return Err(error("HLS: byte-range segments are not supported"));
        } else if line.starts_with("#EXT-X-KEY") && !line.contains("METHOD=NONE") {
            return Err(error("HLS: encrypted segments are not supported"));
        } else if line == "#EXT-X-ENDLIST" || line == "#EXT-X-PLAYLIST-TYPE:VOD" {
            vod = true;
        } else if !line.starts_with('#') {
            segments.push((resolve(line)?, duration.take().ok_or_else(|| error("HLS: segment without EXTINF"))?));
        }
    }
    if !vod { return Err(error("HLS: live playlists are not supported")); }
    if segments.is_empty() { return Err(error("HLS: playlist has no segments")); }
    Ok(Playlist { init, segments })
}

async fn fetch(client: Client, url: String, limit: usize, cancelled: Arc<AtomicBool>) -> io::Result<Vec<u8>> {
    let mut last = error("HLS: download failed");
    // One retry absorbs a dropped mobile connection without hiding a real 403/404.
    for attempt in 0..2 {
        if cancelled.load(Ordering::Relaxed) { return Err(error("download cancelled")); }
        if attempt > 0 { tokio::time::sleep(Duration::from_millis(500)).await; }
        let mut response = match client.get(&url).send().await.and_then(|r| r.error_for_status()) {
            Ok(response) => response,
            // HTTP status errors (expired signature, 404) are final.
            Err(cause) if cause.status().is_some() => return Err(error(cause.without_url())),
            Err(cause) => { last = error(cause.without_url()); continue; }
        };
        let mut bytes = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if cancelled.load(Ordering::Relaxed) { return Err(error("download cancelled")); }
                    if bytes.len() + chunk.len() > limit { return Err(error("HLS: oversized response")); }
                    bytes.extend_from_slice(&chunk);
                }
                Ok(None) => return Ok(bytes),
                Err(cause) => { last = error(cause.without_url()); break; }
            }
        }
    }
    Err(last)
}

/// Index of the segment containing `position` and the offset into it.
pub fn start_segment(segments: &[(String, f64)], position: Duration) -> (usize, Duration) {
    let target = position.as_secs_f64();
    let mut elapsed = 0.0;
    for (index, (_, length)) in segments.iter().enumerate() {
        if elapsed + length > target || index + 1 == segments.len() {
            return (index, Duration::from_secs_f64((target - elapsed).max(0.0)));
        }
        elapsed += length;
    }
    (0, Duration::ZERO)
}

pub struct HlsReader {
    client: Client,
    runtime: Handle,
    cancelled: Arc<AtomicBool>,
    pending: VecDeque<String>,
    prefetch: Option<JoinHandle<io::Result<Vec<u8>>>>,
    current: Vec<u8>,
    offset: usize,
    position: u64,
}

impl HlsReader {
    /// Call only from a decoder thread. Returns the reader starting at the
    /// segment containing `position` and the remaining offset to skip in PCM.
    pub fn open(client: Client, url: &str, runtime: Handle, cancelled: Arc<AtomicBool>, position: Duration)
        -> io::Result<(Self, Duration)> {
        let base = Url::parse(url).map_err(|_| error("HLS: invalid playlist URL"))?;
        let text = runtime.block_on(fetch(client.clone(), url.into(), MAX_PLAYLIST, cancelled.clone()))?;
        let playlist = parse(&base, &String::from_utf8_lossy(&text))?;
        let (first, skip) = start_segment(&playlist.segments, position);
        let mut pending: VecDeque<String> = playlist.init.into_iter().collect();
        pending.extend(playlist.segments.into_iter().skip(first).map(|(url, _)| url));
        let mut reader = Self { client, runtime, cancelled, pending, prefetch: None, current: Vec::new(), offset: 0, position: 0 };
        reader.spawn_next();
        reader.advance()?;
        Ok((reader, skip))
    }

    fn spawn_next(&mut self) {
        if let Some(url) = self.pending.pop_front() {
            self.prefetch = Some(self.runtime.spawn(fetch(self.client.clone(), url, MAX_SEGMENT, self.cancelled.clone())));
        }
    }

    /// Replace the consumed segment with the prefetched one; false at the end.
    fn advance(&mut self) -> io::Result<bool> {
        let Some(task) = self.prefetch.take() else { return Ok(false) };
        self.current = self.runtime.block_on(task).map_err(error)??;
        self.offset = 0;
        self.spawn_next();
        Ok(true)
    }
}

impl Read for HlsReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.cancelled.load(Ordering::Relaxed) { return Err(error("download cancelled")); }
        while self.offset >= self.current.len() {
            if !self.advance()? { return Ok(0); }
        }
        let n = output.len().min(self.current.len() - self.offset);
        output[..n].copy_from_slice(&self.current[self.offset..self.offset + n]);
        self.offset += n;
        self.position += n as u64;
        Ok(n)
    }
}

impl Seek for HlsReader {
    /// Forward-only: position queries succeed, real seeks are reported.
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        match from {
            SeekFrom::Current(0) => Ok(self.position),
            SeekFrom::Start(n) if n == self.position => Ok(self.position),
            _ => Err(io::Error::new(io::ErrorKind::Unsupported, "HLS stream is not byte-seekable")),
        }
    }
}

impl Drop for HlsReader {
    fn drop(&mut self) { if let Some(task) = self.prefetch.take() { task.abort(); } }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url { Url::parse("https://cdn.example/a/aac_160k/id/playlist.m3u8?sig=1").unwrap() }

    #[test]
    fn parses_soundcloud_fmp4_vod() {
        // Sanitized shape of a live SoundCloud aac_160k media playlist.
        let text = "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:10\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n\
            #EXT-X-MAP:URI=\"https://cdn.example/a/init.mp4?expires=1&Signature=x\"\n\
            #EXTINF:10.007800,\nhttps://cdn.example/a/data000.m4s?expires=1\n#EXTINF:9.984580,\ndata001.m4s?expires=1\n#EXT-X-ENDLIST\n";
        let playlist = parse(&base(), text).unwrap();
        assert_eq!(playlist.init.as_deref(), Some("https://cdn.example/a/init.mp4?expires=1&Signature=x"));
        assert_eq!(playlist.segments.len(), 2);
        assert_eq!(playlist.segments[1].0, "https://cdn.example/a/aac_160k/id/data001.m4s?expires=1");
        assert!((playlist.segments[0].1 - 10.0078).abs() < 1e-9);
    }

    #[test]
    fn rejects_unsupported_playlists() {
        for text in [
            "not a playlist",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nlow.m3u8\n",
            "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\n#EXTINF:1,\na.ts\n#EXT-X-ENDLIST\n",
            "#EXTM3U\n#EXT-X-BYTERANGE:100@0\n#EXTINF:1,\na.ts\n#EXT-X-ENDLIST\n",
            "#EXTM3U\n#EXTINF:1,\na.ts\n",
            "#EXTM3U\n#EXT-X-ENDLIST\n",
            "#EXTM3U\na.ts\n#EXT-X-ENDLIST\n",
            "#EXTM3U\n#EXTINF:1,\nhttp://insecure.example/a.ts\n#EXT-X-ENDLIST\n",
        ] {
            assert!(parse(&base(), text).is_err(), "{text}");
        }
        assert!(parse(&base(), "#EXTM3U\n#EXT-X-KEY:METHOD=NONE\n#EXTINF:1,\na.mp3\n#EXT-X-ENDLIST\n").is_ok());
    }

    #[test]
    fn start_segment_covers_boundaries() {
        let segments: Vec<_> = [10.0, 10.0, 5.0].iter().map(|d| (String::new(), *d)).collect();
        assert_eq!(start_segment(&segments, Duration::ZERO), (0, Duration::ZERO));
        assert_eq!(start_segment(&segments, Duration::from_secs(10)), (1, Duration::ZERO));
        assert_eq!(start_segment(&segments, Duration::from_millis(12_500)), (1, Duration::from_millis(2500)));
        assert_eq!(start_segment(&segments, Duration::from_secs(99)).0, 2);
    }
}
