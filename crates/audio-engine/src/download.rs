//! Save a resolved stream to a local file (progressive media or HLS segments).
//! Progressive media is fetched in sequential byte ranges (googlevideo
//! throttles un-ranged full downloads); HLS writes the init map followed by
//! every segment, which yields a valid fragmented MP4 (or MP3/Ogg) file.
use crate::{hls, EngineError, Result};
use core_types::{AudioCodec, StreamInfo, StreamSource};
use reqwest::{header, Client, Response, StatusCode};
use std::{io::SeekFrom, path::Path, sync::atomic::{AtomicBool, Ordering}, time::{Duration, Instant}};
use tokio::{fs::File, io::{AsyncSeekExt, AsyncWriteExt, BufWriter}};
use url::Url;

const CHUNK: u64 = 2 * 1024 * 1024;
const MAX_FILE: u64 = 1024 * 1024 * 1024;
const MAX_PLAYLIST: u64 = 4 * 1024 * 1024;
const MAX_SEGMENT: u64 = 32 * 1024 * 1024;
const REPORT_EVERY: Duration = Duration::from_millis(250);

/// File extension (without dot) for the container this stream will be saved as.
pub fn file_extension(stream: &StreamInfo) -> &'static str {
    if stream.source == StreamSource::Local {
        // Keep what is already on disk when it is a container we know.
        let ext = Path::new(&stream.stream_url).extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
        if let Some(known) = ["mp3", "m4a", "mp4", "webm", "opus", "ogg"].into_iter().find(|k| ext.as_deref() == Some(*k)) {
            return known;
        }
    }
    match (stream.codec, stream.source) {
        (AudioCodec::Mp3, _) => "mp3",
        (AudioCodec::Aac, _) => "m4a",
        // SoundCloud Opus is Ogg Opus HLS; YouTube itag 251 is WebM.
        (AudioCodec::Opus, StreamSource::SoundCloud) => "opus",
        (AudioCodec::Opus, _) => "webm",
    }
}

/// Download `stream` into `destination` (the caller passes a temporary `.part`
/// path and renames it afterwards). `progress(done_bytes, total_bytes)` is
/// called from the download task. Returns the number of bytes written.
pub async fn download(client: &reqwest::Client, stream: &StreamInfo, destination: &Path,
    cancelled: &AtomicBool, progress: impl FnMut(u64, Option<u64>) + Send) -> Result<u64> {
    if stream.source == StreamSource::Local { return Err(message("stream is already a local file")); }
    let url = Url::parse(&stream.stream_url).ok().filter(|u| matches!(u.scheme(), "https" | "http"))
        .ok_or_else(|| message("download needs an HTTP(S) stream URL"))?;
    // HLS segment URLs are checked before the destination is created.
    let playlist = if stream.hls {
        let text = playlist(client, url.as_str(), cancelled).await?;
        Some(hls::parse(&url, &String::from_utf8_lossy(&text))?)
    } else { None };
    let file = File::create(destination).await?;
    let mut out = Output { file: BufWriter::with_capacity(256 * 1024, file), written: 0, total: None,
        cancelled, report: progress, reported: None };
    match playlist {
        Some(playlist) => {
            for url in playlist.init.iter().chain(playlist.segments.iter().map(|(url, _)| url)) {
                out.check()?;
                retry(client, Request::Segment(url), &mut out).await?;
            }
        }
        None => loop {
            out.check()?;
            let start = out.written;
            let end = out.total.map_or(start + CHUNK, |total| (start + CHUNK).min(total)) - 1;
            let complete = retry(client, Request::Range(url.as_str(), start, end), &mut out).await?;
            if complete || out.total.is_some_and(|total| out.written >= total) { break; }
        },
    }
    out.finish().await
}

fn message(text: &str) -> EngineError { EngineError::Message(text.into()) }

enum Failure { Transient(EngineError), Final(EngineError) }

/// Never expose signed URLs (including the listener's IP) in UI errors.
fn transport(cause: reqwest::Error) -> Failure {
    let status = cause.status().is_some();
    let error = EngineError::Http(cause.without_url());
    // HTTP status errors (expired signature, 403, 404) are final.
    if status { Failure::Final(error) } else { Failure::Transient(error) }
}

/// `bytes first-last/total`; `*` totals are rejected (length must be known).
fn content_range(value: &str) -> Option<(u64, u64, u64)> {
    let (bounds, total) = value.trim().strip_prefix("bytes ")?.split_once('/')?;
    let (first, last) = bounds.split_once('-')?;
    Some((first.trim().parse().ok()?, last.trim().parse().ok()?, total.trim().parse().ok()?))
}

/// Validate a 206 answer to `bytes=start-end`; returns (length, total).
fn accept_range(value: Option<&str>, start: u64, end: u64) -> std::result::Result<(u64, u64), &'static str> {
    let (first, last, total) = value.and_then(content_range).ok_or("invalid Content-Range")?;
    if first != start || last < first || last > end || last >= total {
        return Err("server returned an unexpected byte range");
    }
    // Servers may cut a range short, but never before the end of the file.
    if last != end && last + 1 != total { return Err("server returned a short byte range"); }
    Ok((last - first + 1, total))
}

struct Output<'a, F> {
    file: BufWriter<File>,
    written: u64,
    total: Option<u64>,
    cancelled: &'a AtomicBool,
    report: F,
    reported: Option<Instant>,
}

impl<F: FnMut(u64, Option<u64>)> Output<'_, F> {
    fn check(&self) -> Result<()> {
        if self.cancelled.load(Ordering::Relaxed) { Err(message("download cancelled")) } else { Ok(()) }
    }

    async fn write(&mut self, bytes: &[u8]) -> Result<()> {
        if self.written + bytes.len() as u64 > MAX_FILE { return Err(message("download exceeds the 1 GiB size limit")); }
        self.file.write_all(bytes).await?;
        self.written += bytes.len() as u64;
        if self.reported.is_none_or(|at| at.elapsed() >= REPORT_EVERY) {
            self.reported = Some(Instant::now());
            (self.report)(self.written, self.total);
        }
        Ok(())
    }

    /// Drop a partially written attempt before retrying it.
    async fn rewind(&mut self, to: u64) -> Result<()> {
        if self.written == to { return Ok(()); }
        self.file.flush().await?;
        let file = self.file.get_mut();
        file.set_len(to).await?;
        file.seek(SeekFrom::Start(to)).await?;
        self.written = to;
        Ok(())
    }

    async fn finish(mut self) -> Result<u64> {
        self.file.flush().await?;
        self.file.get_mut().sync_all().await?;
        (self.report)(self.written, Some(self.written));
        Ok(self.written)
    }

    /// Stream the body to disk; returns its length. Cancellation is final.
    async fn copy(&mut self, response: &mut Response, limit: u64) -> std::result::Result<u64, Failure> {
        let mut length = 0u64;
        while let Some(chunk) = response.chunk().await.map_err(transport)? {
            self.check().map_err(Failure::Final)?;
            length += chunk.len() as u64;
            if length > limit { return Err(Failure::Final(message("oversized download response"))); }
            self.write(&chunk).await.map_err(Failure::Final)?;
        }
        Ok(length)
    }
}

#[derive(Clone, Copy)]
enum Request<'a> { Segment(&'a str), Range(&'a str, u64, u64) }

/// One retry absorbs a dropped mobile connection without hiding a real 403/404.
/// Returns true when the whole resource has been written (200 to a range).
async fn retry<F: FnMut(u64, Option<u64>)>(client: &Client, request: Request<'_>, out: &mut Output<'_, F>) -> Result<bool> {
    let start = out.written;
    let mut last = message("download failed");
    for attempt in 0..2 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            out.check()?;
            out.rewind(start).await?;
        }
        match once(client, request, out).await {
            Ok(complete) => return Ok(complete),
            Err(Failure::Transient(cause)) => last = cause,
            Err(Failure::Final(cause)) => return Err(cause),
        }
    }
    Err(last)
}

async fn once<F: FnMut(u64, Option<u64>)>(client: &Client, request: Request<'_>, out: &mut Output<'_, F>)
    -> std::result::Result<bool, Failure> {
    let fatal = |text: &str| Failure::Final(message(text));
    let (url, range) = match request { Request::Segment(url) => (url, None), Request::Range(url, a, b) => (url, Some((a, b))) };
    let mut builder = client.get(url).header(header::ACCEPT_ENCODING, "identity");
    if let Some((start, end)) = range { builder = builder.header(header::RANGE, format!("bytes={start}-{end}")); }
    let mut response = builder.send().await.and_then(|r| r.error_for_status()).map_err(transport)?;
    let declared = response.content_length();
    let truncated = |length: u64| declared.is_some_and(|d| d != length);
    match (range, response.status()) {
        (None, _) => {
            if declared.is_some_and(|d| d > MAX_SEGMENT) { return Err(fatal("oversized HLS segment")); }
            let length = out.copy(&mut response, MAX_SEGMENT).await?;
            if truncated(length) { return Err(Failure::Transient(message("truncated HLS segment"))); }
            Ok(true)
        }
        (Some((start, end)), StatusCode::PARTIAL_CONTENT) => {
            let value = response.headers().get(header::CONTENT_RANGE).and_then(|h| h.to_str().ok());
            let (expected, total) = accept_range(value, start, end).map_err(fatal)?;
            if total > MAX_FILE { return Err(fatal("download exceeds the 1 GiB size limit")); }
            if out.total.is_some_and(|known| known != total) { return Err(fatal("resource length changed during download")); }
            out.total = Some(total);
            let length = out.copy(&mut response, expected).await?;
            if length != expected { return Err(Failure::Transient(message("truncated range response"))); }
            Ok(false)
        }
        // The server ignored Range and sent the whole file.
        (Some((0, _)), StatusCode::OK) => {
            if declared.is_some_and(|d| d > MAX_FILE) { return Err(fatal("download exceeds the 1 GiB size limit")); }
            out.total = declared;
            let length = out.copy(&mut response, MAX_FILE).await?;
            if truncated(length) { return Err(Failure::Transient(message("truncated download"))); }
            Ok(true)
        }
        _ => Err(fatal("server stopped honoring HTTP byte ranges")),
    }
}

async fn playlist(client: &Client, url: &str, cancelled: &AtomicBool) -> Result<Vec<u8>> {
    let mut last = message("HLS: playlist download failed");
    for attempt in 0..2 {
        if cancelled.load(Ordering::Relaxed) { return Err(message("download cancelled")); }
        if attempt > 0 { tokio::time::sleep(Duration::from_millis(500)).await; }
        let mut response = match client.get(url).send().await.and_then(|r| r.error_for_status()).map_err(transport) {
            Ok(response) => response,
            Err(Failure::Final(cause)) => return Err(cause),
            Err(Failure::Transient(cause)) => { last = cause; continue; }
        };
        let mut bytes = Vec::new();
        loop {
            match response.chunk().await.map_err(transport) {
                Ok(Some(chunk)) => {
                    if bytes.len() as u64 + chunk.len() as u64 > MAX_PLAYLIST { return Err(message("HLS: oversized playlist")); }
                    bytes.extend_from_slice(&chunk);
                }
                Ok(None) => return Ok(bytes),
                Err(Failure::Final(cause)) => return Err(cause),
                Err(Failure::Transient(cause)) => { last = cause; break; }
            }
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(codec: AudioCodec, source: StreamSource, url: &str) -> StreamInfo {
        StreamInfo { stream_url: url.into(), codec, bitrate: None, source, expires_at: None, hls: false }
    }

    #[test]
    fn file_extension_matches_the_saved_container() {
        use AudioCodec::*;
        use StreamSource::*;
        for (codec, source, url, expected) in [
            (Mp3, SoundCloud, "https://cdn.example/a", "mp3"),
            (Aac, SoundCloud, "https://cdn.example/a", "m4a"),
            (Aac, YouTube, "https://cdn.example/a", "m4a"),
            (Opus, YouTube, "https://cdn.example/a", "webm"),
            (Opus, SoundCloud, "https://cdn.example/a", "opus"),
            (Opus, Local, "/music/a.WEBM", "webm"),
            (Opus, Local, "/music/a.opus", "opus"),
            (Aac, Local, "/music/a.mp4", "mp4"),
            (Aac, Local, "/music/a.part", "m4a"),
            (Opus, Local, "/music/a", "webm"),
        ] {
            assert_eq!(file_extension(&stream(codec, source, url)), expected, "{url}");
        }
    }

    #[test]
    fn content_range_parses_known_lengths_only() {
        assert_eq!(content_range("bytes 0-1023/4096"), Some((0, 1023, 4096)));
        assert_eq!(content_range(" bytes 10-20/21 "), Some((10, 20, 21)));
        assert_eq!(content_range("bytes 0-1023/*"), None);
        assert_eq!(content_range("bytes */4096"), None);
        assert_eq!(content_range("items 0-1/2"), None);
    }

    #[test]
    fn accept_range_rejects_misaligned_answers() {
        assert_eq!(accept_range(Some("bytes 0-99/1000"), 0, 99), Ok((100, 1000)));
        // The last chunk ends at the end of the file.
        assert_eq!(accept_range(Some("bytes 900-999/1000"), 900, 1999), Ok((100, 1000)));
        for (value, start, end) in [
            (None, 0, 99),
            (Some("bytes 1-100/1000"), 0, 99),
            (Some("bytes 0-199/1000"), 0, 99),
            (Some("bytes 0-49/1000"), 0, 99),
            (Some("bytes 0-1000/1000"), 0, 2000),
            (Some("bytes 50-10/1000"), 50, 99),
        ] {
            assert!(accept_range(value, start, end).is_err(), "{value:?}");
        }
    }
}
