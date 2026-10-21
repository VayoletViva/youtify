//! Seekable HTTP range windows. Network futures run on Tokio, never the
//! audio callback. At most three 256 KiB windows exist per decoder, including
//! an in-flight prefetch. Servers must support byte ranges and a known length.
use reqwest::{header, Client, StatusCode};
use std::{io::{self, Read, Seek, SeekFrom}, sync::{Arc, atomic::{AtomicBool, Ordering}}, time::Duration};
use tokio::{runtime::Handle, task::JoinHandle};

const WINDOW: u64 = 256 * 1024;

struct Window { start: u64, total: u64, bytes: Vec<u8> }

fn error(message: impl ToString) -> io::Error { io::Error::other(message.to_string()) }

fn http_error(cause: reqwest::Error) -> io::Error {
    // Never expose signed URLs (including the listener's IP) in UI errors.
    error(cause.without_url())
}

async fn fetch(client: Client, url: String, start: u64, cancelled: Arc<AtomicBool>) -> io::Result<Window> {
    if cancelled.load(Ordering::Relaxed) { return Err(error("download cancelled")); }
    let mut response = client.get(url)
        .header(header::ACCEPT_ENCODING, "identity")
        .header(header::RANGE, format!("bytes={start}-{}", start + WINDOW - 1))
        .send().await.map_err(http_error)?.error_for_status().map_err(http_error)?;
    if response.status() != StatusCode::PARTIAL_CONTENT {
        return Err(error("server does not support HTTP byte ranges; use another decoder backend"));
    }
    let range = response.headers().get(header::CONTENT_RANGE)
        .and_then(|h| h.to_str().ok()).ok_or_else(|| error("missing Content-Range"))?;
    let (bounds, total) = range.strip_prefix("bytes ").and_then(|r| r.split_once('/'))
        .ok_or_else(|| error("invalid Content-Range"))?;
    let (first, last) = bounds.split_once('-').ok_or_else(|| error("invalid range bounds"))?;
    let first: u64 = first.parse().map_err(error)?;
    let last: u64 = last.parse().map_err(error)?;
    let total: u64 = total.parse().map_err(error)?;
    if first != start || last < first || last >= total || last - first >= WINDOW
        || last != (start + WINDOW - 1).min(total.saturating_sub(1)) {
        return Err(error("server returned an unexpected byte range"));
    }
    let expected = (last - first + 1) as usize;
    let mut bytes = Vec::with_capacity(expected);
    while let Some(chunk) = response.chunk().await.map_err(http_error)? {
        if cancelled.load(Ordering::Relaxed) { return Err(error("download cancelled")); }
        if bytes.len() + chunk.len() > expected { return Err(error("oversized range response")); }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.len() != expected { return Err(error("truncated range response")); }
    Ok(Window { start, total, bytes })
}

pub struct HttpReader {
    client: Client,
    url: String,
    runtime: Handle,
    cancelled: Arc<AtomicBool>,
    current: Window,
    previous: Option<Window>,
    prefetch: Option<(u64, JoinHandle<io::Result<Window>>)>,
    position: u64,
}

impl HttpReader {
    /// Call only from a dedicated decoder thread, not a Tokio worker or CPAL.
    pub fn open(client: Client, url: String, runtime: Handle, cancelled: Arc<AtomicBool>) -> io::Result<Self> {
        let current = runtime.block_on(fetch(client.clone(), url.clone(), 0, cancelled.clone()))?;
        let mut reader = Self { client, url, runtime, cancelled, current, previous: None, prefetch: None, position: 0 };
        reader.prefetch();
        Ok(reader)
    }

    pub fn len(&self) -> u64 { self.current.total }
    pub fn is_empty(&self) -> bool { self.len() == 0 }

    pub fn client() -> io::Result<Client> {
        Client::builder().user_agent(core_types::MEDIA_USER_AGENT).connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30)).no_gzip().no_brotli().build().map_err(error)
    }

    fn prefetch(&mut self) {
        if let Some((_, task)) = self.prefetch.take() { task.abort(); }
        let start = self.current.start + self.current.bytes.len() as u64;
        if start < self.len() {
            let task = self.runtime.spawn(fetch(self.client.clone(), self.url.clone(), start, self.cancelled.clone()));
            self.prefetch = Some((start, task));
        }
    }

    fn window(&mut self) -> io::Result<()> {
        if self.position >= self.current.start && self.position < self.current.start + self.current.bytes.len() as u64 { return Ok(()); }
        if self.previous.as_ref().is_some_and(|w| self.position >= w.start && self.position < w.start + w.bytes.len() as u64) {
            std::mem::swap(&mut self.current, self.previous.as_mut().unwrap());
            self.prefetch();
            return Ok(());
        }
        let start = self.position / WINDOW * WINDOW;
        // Drop the old back-window before allocating a new one.
        self.previous = None;
        let next = match self.prefetch.take() {
            Some((offset, task)) if offset == start => self.runtime.block_on(task).map_err(error)??,
            other => {
                if let Some((_, task)) = other { task.abort(); }
                self.runtime.block_on(fetch(self.client.clone(), self.url.clone(), start, self.cancelled.clone()))?
            }
        };
        if next.total != self.len() { return Err(error("resource length changed during playback")); }
        self.previous = Some(std::mem::replace(&mut self.current, next));
        self.prefetch();
        Ok(())
    }
}

impl Read for HttpReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.cancelled.load(Ordering::Relaxed) { return Err(error("download cancelled")); }
        if output.is_empty() || self.position >= self.len() { return Ok(0); }
        self.window()?;
        let offset = (self.position - self.current.start) as usize;
        let n = output.len().min(self.current.bytes.len() - offset);
        output[..n].copy_from_slice(&self.current.bytes[offset..offset + n]);
        self.position += n as u64;
        Ok(n)
    }
}

impl Seek for HttpReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let position = match from {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::Current(n) => self.position as i128 + n as i128,
            SeekFrom::End(n) => self.len() as i128 + n as i128,
        };
        if !(0..=u64::MAX as i128).contains(&position) { return Err(error("invalid seek position")); }
        self.position = position as u64;
        Ok(self.position)
    }
}

impl Drop for HttpReader {
    fn drop(&mut self) { if let Some((_, task)) = self.prefetch.take() { task.abort(); } }
}