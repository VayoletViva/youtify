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

/// Fetch one window. One retry absorbs a dropped mobile connection (send or
/// body error, truncated body); HTTP status and range errors are final.
async fn fetch(client: Client, url: String, start: u64, cancelled: Arc<AtomicBool>) -> io::Result<Window> {
    let mut last = error("download failed");
    for attempt in 0..2 {
        if attempt > 0 { tokio::time::sleep(Duration::from_millis(500)).await; }
        match fetch_once(&client, &url, start, &cancelled).await {
            Ok(window) => return Ok(window),
            Err(Failure::Transient(cause)) => last = cause,
            Err(Failure::Final(cause)) => return Err(cause),
        }
    }
    Err(last)
}

enum Failure { Transient(io::Error), Final(io::Error) }

fn transport(cause: reqwest::Error) -> Failure {
    if cause.status().is_some() { Failure::Final(http_error(cause)) } else { Failure::Transient(http_error(cause)) }
}

async fn fetch_once(client: &Client, url: &str, start: u64, cancelled: &AtomicBool) -> Result<Window, Failure> {
    let fatal = |message: &str| Failure::Final(error(message));
    if cancelled.load(Ordering::Relaxed) { return Err(fatal("download cancelled")); }
    let mut response = client.get(url)
        .header(header::ACCEPT_ENCODING, "identity")
        .header(header::RANGE, format!("bytes={start}-{}", start + WINDOW - 1))
        .send().await.and_then(|r| r.error_for_status()).map_err(transport)?;
    if response.status() != StatusCode::PARTIAL_CONTENT {
        return Err(fatal("server does not support HTTP byte ranges; use another decoder backend"));
    }
    let range = response.headers().get(header::CONTENT_RANGE)
        .and_then(|h| h.to_str().ok()).ok_or_else(|| fatal("missing Content-Range"))?;
    let parsed = range.strip_prefix("bytes ").and_then(|r| r.split_once('/'))
        .and_then(|(bounds, total)| {
            let (first, last) = bounds.split_once('-')?;
            Some((first.parse::<u64>().ok()?, last.parse::<u64>().ok()?, total.parse::<u64>().ok()?))
        });
    let (first, last, total) = parsed.ok_or_else(|| fatal("invalid Content-Range"))?;
    if first != start || last < first || last >= total || last - first >= WINDOW
        || last != (start + WINDOW - 1).min(total.saturating_sub(1)) {
        return Err(fatal("server returned an unexpected byte range"));
    }
    let expected = (last - first + 1) as usize;
    let mut bytes = Vec::with_capacity(expected);
    while let Some(chunk) = response.chunk().await.map_err(transport)? {
        if cancelled.load(Ordering::Relaxed) { return Err(fatal("download cancelled")); }
        if bytes.len() + chunk.len() > expected { return Err(fatal("oversized range response")); }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.len() != expected { return Err(Failure::Transient(error("truncated range response"))); }
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
    /// Bytes of the first window; valid right after `open`.
    pub fn head(&self) -> &[u8] { if self.current.start == 0 { &self.current.bytes } else { &[] } }
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

/// Forward-only view of `[0, init_end)` followed by `[resume, len)`, used to
/// start fragmented MP4 at a fragment without Symphonia's full-file atom scan.
/// Like `HlsReader`, position queries succeed and real seeks are reported.
pub struct SplicedReader { inner: HttpReader, init_end: u64, resume: u64, jumped: bool, position: u64 }

impl SplicedReader {
    pub fn new(inner: HttpReader, init_end: u64, resume: u64) -> Self {
        let jumped = resume == init_end;
        Self { inner, init_end, resume, jumped, position: 0 }
    }
}

impl Read for SplicedReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let mut limit = output.len();
        if !self.jumped {
            if self.inner.position >= self.init_end {
                self.inner.seek(SeekFrom::Start(self.resume))?;
                self.jumped = true;
            } else {
                limit = limit.min((self.init_end - self.inner.position) as usize);
            }
        }
        let n = self.inner.read(&mut output[..limit])?;
        self.position += n as u64;
        Ok(n)
    }
}

impl Seek for SplicedReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        match from {
            SeekFrom::Current(0) => Ok(self.position),
            SeekFrom::Start(n) if n == self.position => Ok(self.position),
            _ => Err(io::Error::new(io::ErrorKind::Unsupported, "stream is opened forward-only")),
        }
    }
}

impl Drop for HttpReader {
    fn drop(&mut self) { if let Some((_, task)) = self.prefetch.take() { task.abort(); } }
}