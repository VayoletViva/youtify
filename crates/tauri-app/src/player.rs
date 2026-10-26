use std::{collections::{HashSet, VecDeque}, path::{Path, PathBuf},
    sync::{atomic::AtomicBool, Arc, Mutex, RwLock}, time::{Duration, Instant}};
use audio_engine::{decoder::FfmpegDecoder, streamer::HttpReader, AudioEngine, PlayerCommand, PlayerEvent};
use core_types::{AudioCodec, MetadataProvider, StreamInfo, StreamResolver, StreamSource, TrackMetadata};
use resolvers::{spotify::Spotify as SpotifyResolver, youtube::YouTube as YoutubeResolver,
    soundcloud::SoundCloud as SoundCloudResolver, deezer::Deezer as DeezerResolver};
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::{broadcast, mpsc, oneshot, Semaphore};
use wave_engine::{features::vectorize, next_wave_mix, unix_timestamp, Interaction, UserTasteProfile, WaveConfig, WaveDb};

/// Ask for online discoveries when fewer unplayed, unqueued tracks remain.
const DISCOVERY_LOW_WATER: usize = 8;
/// A discovery that added nothing is not retried sooner (no request storms offline).
const DISCOVERY_COOLDOWN: Duration = Duration::from_secs(45);
/// Share of each wave batch reserved for never-played (discovered) tracks.
const FRESH_RATIO: f32 = 0.5;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
const EMPTY_WAVE: &str = "Wave library is empty and online discovery found nothing; check the connection";

type Reply<T> = oneshot::Sender<Result<T>>;
type Result<T> = std::result::Result<T, String>;

fn is_executable(path: &std::path::Path) -> bool {
    if !path.is_absolute() { return false; }
    let Ok(metadata) = path.metadata() else { return false; };
    if !metadata.is_file() { return false; }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 { return false; }
    }
    #[cfg(windows)]
    if !path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("exe")) { return false; }
    true
}

fn ffmpeg_path(value: Option<std::ffi::OsString>, app_exe: Option<PathBuf>,
    search_path: Option<std::ffi::OsString>, mobile: bool) -> Result<Option<PathBuf>> {
    if let Some(value) = value {
        if mobile { return Err("WAVE_FFMPEG is supported only on desktop".into()); }
        let path = PathBuf::from(value);
        if !is_executable(&path) {
            return Err("WAVE_FFMPEG must be an absolute path to an existing FFmpeg executable (not a directory or command arguments)".into());
        }
        return Ok(Some(path));
    }
    if mobile { return Ok(None); }
    let name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    if let Some(path) = app_exe.as_deref().and_then(|app| app.parent()).map(|dir| dir.join(name)) {
        if is_executable(&path) { return Ok(Some(path)); }
    }
    if let Some(search_path) = search_path {
        for dir in std::env::split_paths(&search_path).filter(|dir| dir.is_absolute()) {
            let path = dir.join(name);
            if is_executable(&path) { return Ok(Some(path)); }
        }
    }
    Ok(None)
}

// Keep the IPC reply in the actor, not in an abortable network task.
struct PlaybackReply<T = ()>(Option<Reply<T>>);

impl<T> Default for PlaybackReply<T> {
    fn default() -> Self { Self(None) }
}

impl<T> PlaybackReply<T> {
    fn finish(&mut self, result: Result<T>) {
        if let Some(reply) = self.0.take() { let _ = reply.send(result); }
    }

    fn is_pending(&self) -> bool { self.0.is_some() }
}

impl<T> Drop for PlaybackReply<T> {
    fn drop(&mut self) {
        self.finish(Err("Playback request cancelled".into()));
    }
}

#[derive(Clone, Default, Serialize)]
pub struct Snapshot {
    revision: u64,
    track: Option<TrackMetadata>,
    position_ms: u64,
    duration_ms: u64,
    paused: bool,
    volume: f32,
    wave: bool,
    next_track: Option<TrackMetadata>,
    preload: String,
    queue: Vec<TrackMetadata>,
    error: Option<String>,
    /// My Wave is fetching new tracks online.
    pub discovering: bool,
}

#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum UiEvent {
    State { state: Snapshot },
    Gapless { from_track_id: String, to_track_id: String },
    Error { message: String },
    Download { track_id: String, status: &'static str, done_bytes: u64, total_bytes: Option<u64>, message: Option<String> },
}

fn emit_download(app: &AppHandle, track_id: &str, status: &'static str, done_bytes: u64,
    total_bytes: Option<u64>, message: Option<String>) {
    let _ = app.emit("player-event", UiEvent::Download {
        track_id: track_id.into(), status, done_bytes, total_bytes, message });
}

#[derive(Serialize)]
pub struct DownloadItem {
    pub track: TrackMetadata,
    pub path: String,
    pub bytes: u64,
}

#[derive(Serialize)]
pub struct DownloadList {
    pub dir: String,
    pub items: Vec<DownloadItem>,
}

struct Providers {
    spotify: SpotifyResolver,
    youtube: YoutubeResolver,
    soundcloud: SoundCloudResolver,
    deezer: DeezerResolver,
    searches: Semaphore,
}

impl Providers {
    async fn resolve(&self, track: &TrackMetadata) -> Result<StreamInfo> {
        // Prefer the original recording over a cross-provider search match.
        if track.id.starts_with("soundcloud:track:") {
            let first = match resolve_provider("SoundCloud", self.soundcloud.resolve(track)).await {
                Ok(stream) => return Ok(stream),
                Err(error) => error,
            };
            return resolve_provider("YouTube", self.youtube.resolve(track)).await
                .map_err(|error| format!("{first}; {error}"));
        }
        match resolve_provider("YouTube", self.youtube.resolve(track)).await {
            Ok(stream) => Ok(stream),
            Err(first) => resolve_provider("SoundCloud", self.soundcloud.resolve(track)).await
                .map_err(|error| format!("{first}; {error}")),
        }
    }
}

// Catch unwinding at the provider boundary, including a panic after an await.
// Never poll a future again after it panics. Dropping this wrapper still cancels
// the request, unlike spawning a detached provider task on every attempt.
async fn resolve_provider<F>(service: &'static str, future: F) -> Result<StreamInfo>
where F: std::future::Future<Output = std::result::Result<StreamInfo, core_types::ResolverError>> {
    let mut future = Box::pin(future);
    let guarded = std::future::poll_fn(move |cx| {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| future.as_mut().poll(cx))) {
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Ok(std::task::Poll::Ready(result)) => std::task::Poll::Ready(result.map_err(|e| e.to_string())),
            Err(payload) => std::task::Poll::Ready(Err(format!("{service}: resolver panic ({}); playback worker is still running",
                panic_category(payload.as_ref())))),
        }
    });
    tokio::time::timeout(Duration::from_secs(20), guarded).await
        .unwrap_or_else(|_| Err(format!("{service} timeout")))
}

fn panic_category(payload: &(dyn std::any::Any + Send)) -> &'static str {
    // Only fixed labels reach IPC. Panic payloads can contain whole responses,
    // signed media URLs or credentials, even when reqwest errors strip URLs.
    let text = payload.downcast_ref::<String>().map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied()).unwrap_or("");
    if text.contains("not a char boundary") { "UTF-8 string boundary" }
    else if text.contains("index out of bounds") { "index out of bounds" }
    else if text.contains("called `Option::unwrap()`") { "unwrap on None" }
    else if text.contains("called `Result::unwrap()`") { "unwrap on Err" }
    else if text.contains("overflow") { "arithmetic overflow" }
    else { "internal panic; console panic location needed (omit URLs and tokens)" }
}

/// Spotify failures that say nothing about the query itself: regional block,
/// anonymous quota, or no reachable session. Text search may use another provider.
fn spotify_fallback(error: &core_types::ResolverError) -> bool {
    use core_types::ResolverError::*;
    matches!(error, Http { status: 451 | 429 | 403 | 401, .. } | RateLimited { .. } | Unavailable { .. } | Transport { .. })
}

/// Deezer link, URI or bare numeric ID (the resolver would treat text as a search).
fn is_deezer_reference(input: &str) -> bool {
    let lower = input.trim().to_ascii_lowercase();
    lower.contains("://") || lower.starts_with("deezer:") || lower.starts_with("deezer.com/")
        || lower.starts_with("www.deezer.com/") || lower.starts_with("link.deezer.com/")
        || (!lower.is_empty() && lower.len() <= 20 && lower.bytes().all(|b| b.is_ascii_digit()))
}

/// Same split as `SoundCloud::track`: links and IDs are looked up, anything else searched.
fn is_soundcloud_reference(input: &str) -> bool {
    let input = input.trim();
    input.contains("://") || input.starts_with("soundcloud:")
}

/// Join the errors of every attempted provider, so the UI can tell a regional
/// block from "nothing found anywhere".
fn joined(errors: &[String]) -> String { errors.join("; ") }

fn spotify_error(error: core_types::ResolverError) -> String {
    match error {
        core_types::ResolverError::Http { service, status: 451 } => format!("{service}: HTTP 451 (unavailable in this country)"),
        error => error.to_string(),
    }
}

/// Tauri requires Send + Sync state. AudioEngine's device handle is deliberately
/// NOT moved across threads: the actor owns it and SQLite on one dedicated thread.
/// The shared facade owns the actor's bounded mailbox, providers and UI snapshot.
pub struct AppState {
    providers: Arc<Providers>,
    tx: mpsc::Sender<Request>,
    pub snapshot: Arc<RwLock<Snapshot>>,
    downloads: Arc<Downloads>,
}

/// Download jobs run outside the actor; only their DB records go through it.
struct Downloads {
    dir: PathBuf,
    active: Mutex<HashSet<String>>,
    slots: Arc<Semaphore>,
}

impl Downloads {
    fn new(dir: PathBuf) -> Self {
        Self { dir, active: Mutex::new(HashSet::new()), slots: Arc::new(Semaphore::new(2)) }
    }

    fn active(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        // The set holds plain IDs; a panic elsewhere cannot leave it inconsistent.
        self.active.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Removes the track from the in-progress set however the job ends.
struct ActiveDownload { downloads: Arc<Downloads>, id: String }

impl Drop for ActiveDownload {
    fn drop(&mut self) { self.downloads.active().remove(&self.id); }
}

/// Deletes a partial file unless the job renamed it (also on task cancellation).
struct PartFile { path: PathBuf, keep: bool }

impl Drop for PartFile {
    fn drop(&mut self) { if !self.keep { let _ = std::fs::remove_file(&self.path); } }
}

pub enum Request {
    Play(TrackMetadata, Reply<()>),
    Control(String, Option<f64>, Reply<()>),
    Wave(Reply<Vec<TrackMetadata>>),
    Store(Vec<TrackMetadata>, Reply<()>),
    /// Store the track; fail with "Already downloaded" if its file still exists.
    PrepareDownload(TrackMetadata, Reply<()>),
    SetDownload(String, PathBuf, u64, Reply<()>),
    RemoveDownload(String, Reply<Option<PathBuf>>),
    Downloads(Reply<Vec<DownloadItem>>),
}

impl AppState {
    pub fn new(app: AppHandle, path: PathBuf, download_dir: PathBuf) -> std::result::Result<Self, Box<dyn std::error::Error>> {
        // Read once so provider format selection and the decoder cannot disagree.
        let ffmpeg = ffmpeg_path(std::env::var_os("WAVE_FFMPEG"), std::env::current_exe().ok(),
            std::env::var_os("PATH"), cfg!(any(target_os = "android", target_os = "ios")))
            .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;
        let providers = Arc::new(Providers {
            spotify: SpotifyResolver::new()?,
            youtube: if ffmpeg.is_some() { YoutubeResolver::new()? } else { YoutubeResolver::new_native()? },
            soundcloud: if ffmpeg.is_some() { SoundCloudResolver::new()? } else { SoundCloudResolver::new_native()? },
            deezer: DeezerResolver::new()?,
            searches: Semaphore::new(2),
        });
        let downloads = Arc::new(Downloads::new(download_dir));
        let snapshot = Arc::new(RwLock::new(Snapshot { volume: 1.0, preload: "idle".into(), ..Default::default() }));
        let (tx, rx) = mpsc::channel(32);
        let worker_providers = providers.clone();
        let worker_snapshot = snapshot.clone();
        std::thread::Builder::new().name("player-state".into()).spawn(move || {
            let result = (|| -> Result<()> {
                let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2)
                    .enable_all().build().map_err(|e| e.to_string())?;
                runtime.block_on(async {
                    let mut worker = Worker::new(app.clone(), worker_providers, worker_snapshot.clone(), path, ffmpeg)?;
                    worker.run(rx).await;
                    Ok(())
                })
            })();
            if let Err(message) = result {
                if let Ok(mut snapshot) = worker_snapshot.write() {
                    snapshot.error = Some(message.clone());
                    snapshot.revision += 1;
                }
                let _ = app.emit("player-event", UiEvent::Error { message });
            }
        })?;
        Ok(Self { providers, tx, snapshot, downloads })
    }

    pub async fn send(&self, request: Request) -> Result<()> {
        self.tx.try_send(request).map_err(|e| format!("Player busy or unavailable: {e}"))
    }

    /// Round trip to the actor for requests whose answer the caller needs.
    async fn ask<T>(&self, request: impl FnOnce(Reply<T>) -> Request) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        self.send(request(tx)).await?;
        rx.await.map_err(|_| self.reply_error())?
    }

    /// Accept a download job and run it in the background; progress goes out as events.
    pub async fn download(&self, app: AppHandle, track: TrackMetadata) -> Result<()> {
        validate_track(&track)?;
        // Claim first: two quick clicks must not both pass the DB check.
        if !self.downloads.active().insert(track.id.clone()) { return Err("Download already in progress".into()); }
        let active = ActiveDownload { downloads: self.downloads.clone(), id: track.id.clone() };
        let id = track.id.clone();
        self.ask(|reply| Request::PrepareDownload(track.clone(), reply)).await?;
        let (providers, downloads, tx) = (self.providers.clone(), self.downloads.clone(), self.tx.clone());
        emit_download(&app, &id, "started", 0, None, None);
        tokio::spawn(async move {
            let _active = active;
            let result = download_job(&app, &providers, &downloads, &tx, &track).await;
            match result {
                Ok(bytes) => emit_download(&app, &track.id, "done", bytes, Some(bytes), None),
                Err(reason) => emit_download(&app, &track.id, "failed", 0, None, Some(format!("Download failed: {reason}"))),
            }
        });
        Ok(())
    }

    pub async fn list_downloads(&self) -> Result<DownloadList> {
        let items = self.ask(Request::Downloads).await?;
        Ok(DownloadList { dir: self.downloads.dir.display().to_string(), items })
    }

    pub async fn delete_download(&self, app: &AppHandle, track_id: String) -> Result<()> {
        if track_id.is_empty() || track_id.len() > 512 { return Err("Invalid track ID".into()); }
        if self.downloads.active().contains(&track_id) { return Err("Download already in progress".into()); }
        let path = self.ask(|reply| Request::RemoveDownload(track_id.clone(), reply)).await?;
        if let Some(path) = path {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("Could not delete the file: {}", error.kind())),
            }
        }
        emit_download(app, &track_id, "removed", 0, None, None);
        Ok(())
    }

    pub fn reply_error(&self) -> String {
        if self.tx.is_closed() {
            self.snapshot.read().ok().and_then(|state| state.error.clone())
                .unwrap_or_else(|| "Player worker unavailable; restart the application".into())
        } else {
            "Playback request cancelled before completion".into()
        }
    }

    pub async fn search(&self, query: String, source: String) -> Result<Vec<TrackMetadata>> {
        let query = query.trim();
        if query.is_empty() || query.len() > 2048 { return Err("Query must contain 1..2048 bytes".into()); }
        self.search_with_metadata(Duration::from_secs(35), async {
            let providers = &self.providers;
            match source.to_ascii_lowercase().as_str() {
                "spotify" if resolvers::spotify::is_text_query(query) => self.spotify_text(query).await,
                "spotify" => self.spotify_link(query).await,
                "spotify-playlist" => match providers.spotify.playlist(query).await {
                    Ok(tracks) => Ok(tracks),
                    // Track lists cannot be fetched anonymously from blocked regions;
                    // oEmbed still names the playlist so the user can find it elsewhere.
                    Err(error) if spotify_fallback(&error) => match providers.spotify.oembed(query, "playlist").await {
                        Ok(embed) => Err(format!("Spotify playlist unavailable in this country: {}", embed.title)),
                        Err(_) => Err(spotify_error(error)),
                    },
                    Err(error) => Err(spotify_error(error)),
                },
                "deezer" if is_deezer_reference(query) => providers.deezer.track(query).await.map(|t| vec![t]).map_err(|e| e.to_string()),
                "deezer" => providers.deezer.search(query, 40).await.map_err(|e| e.to_string()),
                "deezer-playlist" => providers.deezer.playlist(query).await.map_err(|e| e.to_string()),
                "soundcloud" if is_soundcloud_reference(query) => providers.soundcloud.track(query).await.map(|t| vec![t]).map_err(|e| e.to_string()),
                "soundcloud" => providers.soundcloud.search_tracks(query, 30).await.map_err(|e| e.to_string()),
                "soundcloud-playlist" => providers.soundcloud.playlist(query).await.map_err(|e| e.to_string()),
                _ => Err("Source must be spotify, deezer, soundcloud, spotify-playlist, deezer-playlist or soundcloud-playlist; YouTube exposes streams only".into()),
            }
        }).await
    }

    /// Spotify only supplies metadata, so a blocked text search may be answered
    /// by Deezer, then SoundCloud. An empty list counts as a miss.
    async fn spotify_text(&self, query: &str) -> Result<Vec<TrackMetadata>> {
        let providers = &self.providers;
        let error = match providers.spotify.search(query, 30).await {
            Ok(tracks) => return Ok(tracks),
            Err(error) if spotify_fallback(&error) => spotify_error(error),
            Err(error) => return Err(spotify_error(error)),
        };
        let mut errors = vec![error];
        match providers.deezer.search(query, 40).await {
            Ok(tracks) if !tracks.is_empty() => return Ok(tracks),
            Ok(_) => errors.push(format!("Deezer: no match for {query}")),
            Err(error) => errors.push(error.to_string()),
        }
        match providers.soundcloud.search_tracks(query, 30).await {
            Ok(tracks) if !tracks.is_empty() => Ok(tracks),
            Ok(_) => { errors.push(format!("SoundCloud: no match for {query}")); Err(joined(&errors)) }
            Err(error) => { errors.push(error.to_string()); Err(joined(&errors)) }
        }
    }

    /// A blocked Spotify link still has an oEmbed title; Deezer supplies the rest.
    async fn spotify_link(&self, query: &str) -> Result<Vec<TrackMetadata>> {
        let providers = &self.providers;
        let error = match providers.spotify.track(query).await {
            Ok(track) => return Ok(vec![track]),
            Err(error) if spotify_fallback(&error) => spotify_error(error),
            Err(error) => return Err(spotify_error(error)),
        };
        let embed = providers.spotify.oembed(query, "track").await.map_err(|e| format!("{error}; {e}"))?;
        let mut track = providers.deezer.match_title(&embed.title).await.map_err(|e| format!("{error}; {e}"))?;
        if track.cover_url.is_none() { track.cover_url = embed.cover_url; }
        Ok(vec![track])
    }

    async fn search_with_metadata(&self, timeout: Duration,
        metadata: impl std::future::Future<Output = Result<Vec<TrackMetadata>>>) -> Result<Vec<TrackMetadata>> {
        let _permit = self.providers.searches.try_acquire().map_err(|_| "Search busy; try again".to_string())?;
        // One budget includes the actor mailbox and acknowledgement, not just the provider.
        tokio::time::timeout(timeout, async {
            let tracks = metadata.await?;
            if tracks.len() > 2000 { return Err("Playlist exceeds 2000 tracks".into()); }
            let (tx, rx) = oneshot::channel();
            self.send(Request::Store(tracks.clone(), tx)).await?;
            rx.await.map_err(|_| self.reply_error())??;
            Ok(tracks)
        }).await.map_err(|_| "Search timed out".to_string())?
    }
}

/// Resolve like playback, save to `<name>.part`, rename, then record it in the DB.
async fn download_job(app: &AppHandle, providers: &Providers, downloads: &Downloads,
    tx: &mpsc::Sender<Request>, track: &TrackMetadata) -> Result<u64> {
    let _slot = downloads.slots.acquire().await.map_err(|_| "download queue closed".to_string())?;
    // Resolve only after a slot is free: signed URLs expire while queued.
    let stream = providers.resolve(track).await?;
    let client = HttpReader::client().map_err(|e| format!("HTTP client: {}", e.kind()))?;
    let destination = downloads.dir.join(safe_file_name(track, audio_engine::download::file_extension(&stream)));
    let mut part = destination.clone().into_os_string();
    part.push(".part");
    let mut part = PartFile { path: PathBuf::from(part), keep: false };
    let cancelled = AtomicBool::new(false);
    let (progress_app, id) = (app.clone(), track.id.clone());
    let mut last = None::<Instant>;
    let bytes = audio_engine::download::download(&client, &stream, &part.path, &cancelled, move |done, total| {
        if last.is_some_and(|at| at.elapsed() < PROGRESS_INTERVAL) { return; }
        last = Some(Instant::now());
        emit_download(&progress_app, &id, "progress", done, total, None);
    }).await.map_err(download_error)?;
    std::fs::rename(&part.path, &destination).map_err(|e| format!("could not save the file: {}", e.kind()))?;
    part.keep = true;
    let (reply, recorded) = oneshot::channel();
    let recorded = match tx.send(Request::SetDownload(track.id.clone(), destination.clone(), bytes, reply)).await {
        Ok(()) => recorded.await.unwrap_or_else(|_| Err("player stopped".into())),
        Err(_) => Err("player stopped".into()),
    };
    // An unrecorded file could never be listed or deleted from the app.
    if let Err(error) = recorded { let _ = std::fs::remove_file(&destination); return Err(error); }
    Ok(bytes)
}

/// reqwest errors print the request URL, which for media is signed.
fn download_error(error: audio_engine::EngineError) -> String {
    match error {
        audio_engine::EngineError::Http(error) => error.without_url().to_string(),
        audio_engine::EngineError::Io(error) => format!("file error: {}", error.kind()),
        error => error.to_string(),
    }
}

/// `<artists> - <title> [<id>].<ext>`, valid on Windows, Android and Linux.
/// Byte caps keep the name plus ".part" under the common 255-byte limit.
fn safe_file_name(track: &TrackMetadata, extension: &str) -> String {
    let artists = track.artists.iter().map(|a| a.trim()).filter(|a| !a.is_empty()).collect::<Vec<_>>().join(", ");
    let base = if artists.is_empty() { track.title.clone() } else { format!("{artists} - {}", track.title) };
    let base = file_component(&base, 150);
    let id = file_component(&track.id.replace(':', "-"), 48);
    let base = if base.is_empty() { "track".into() } else { base };
    format!("{base} [{id}].{extension}")
}

fn file_component(text: &str, max_bytes: usize) -> String {
    let cleaned: String = text.chars()
        .map(|c| if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .collect();
    let mut out = String::new();
    for c in cleaned.trim_matches(|c: char| c == '.' || c.is_whitespace()).chars() {
        if out.len() + c.len_utf8() > max_bytes { break; }
        out.push(c);
    }
    // Truncation may expose a trailing dot or space, which Windows strips silently.
    out.trim_end_matches(|c: char| c == '.' || c.is_whitespace()).to_string()
}

fn codec_for_extension(extension: &str) -> Option<AudioCodec> {
    match extension.to_ascii_lowercase().as_str() {
        "mp3" => Some(AudioCodec::Mp3),
        "m4a" | "mp4" | "aac" => Some(AudioCodec::Aac),
        "webm" | "opus" | "ogg" => Some(AudioCodec::Opus),
        _ => None,
    }
}

/// A downloaded file as a stream, unless the active decoder cannot play it.
fn local_stream(path: &Path, ffmpeg: bool) -> Option<StreamInfo> {
    let codec = codec_for_extension(path.extension()?.to_str()?)?;
    if codec == AudioCodec::Opus && !ffmpeg { return None; }
    if !path.is_absolute() || !path.is_file() { return None; }
    Some(StreamInfo { stream_url: path.to_str()?.to_owned(), codec, bitrate: None,
        source: StreamSource::Local, expires_at: None, hls: false })
}

fn should_discover(wave: bool, running: bool, unplayed: usize, last_empty: Option<Instant>, now: Instant) -> bool {
    wave && !running && unplayed < DISCOVERY_LOW_WATER
        && last_empty.is_none_or(|at| now.saturating_duration_since(at) >= DISCOVERY_COOLDOWN)
}

struct Discovered {
    tracks: Vec<TrackMetadata>,
    error: Option<String>,
}

/// Runs off the worker thread; providers fail independently and never stop each other.
async fn discover(providers: &Providers, seeds: &[TrackMetadata]) -> Discovered {
    const BUDGET: Duration = Duration::from_secs(25);
    let timeout = |service: &str| format!("{service}: discovery timeout");
    let deezer = async {
        tokio::time::timeout(BUDGET, providers.deezer.discover(seeds, 30)).await
            .map_err(|_| timeout("Deezer")).and_then(|r| r.map_err(|e| e.to_string()))
    };
    let youtube = async {
        let Some(seed) = seeds.first() else { return Vec::new() };
        match tokio::time::timeout(BUDGET, providers.youtube.radio(seed, 20)).await {
            Ok(Ok(tracks)) => tracks,
            _ => Vec::new(),
        }
    };
    let soundcloud = async {
        let Some(seed) = seeds.iter().find(|s| s.id.starts_with("soundcloud:track:")) else { return Vec::new() };
        match tokio::time::timeout(BUDGET, providers.soundcloud.related(&seed.id, 20)).await {
            Ok(Ok(tracks)) => tracks,
            _ => Vec::new(),
        }
    };
    let (deezer, youtube, soundcloud) = tokio::join!(deezer, youtube, soundcloud);
    let (deezer, error) = match deezer { Ok(tracks) => (tracks, None), Err(error) => (Vec::new(), Some(error)) };
    let seen: HashSet<&str> = seeds.iter().map(|s| s.id.as_str()).collect();
    let mut tracks = merge_discoveries(vec![deezer, youtube, soundcloud]);
    tracks.retain(|t| !seen.contains(t.id.as_str()));
    Discovered { tracks, error }
}

/// Round-robin across providers so one source cannot fill the whole batch; dedup by ID.
fn merge_discoveries(lists: Vec<Vec<TrackMetadata>>) -> Vec<TrackMetadata> {
    let mut seen = HashSet::new();
    let mut lists: Vec<_> = lists.into_iter().map(Vec::into_iter).collect();
    let mut merged = Vec::new();
    loop {
        let mut any = false;
        for list in &mut lists {
            if let Some(track) = list.next() {
                any = true;
                if seen.insert(track.id.clone()) { merged.push(track); }
            }
        }
        if !any { return merged; }
    }
}

/// App-level facade over the actual wave-engine API (which exports no WaveEngine).
struct WaveEngine {
    db: WaveDb,
    config: WaveConfig,
    profile: UserTasteProfile,
}

impl WaveEngine {
    fn new(path: PathBuf) -> Result<Self> {
        let db = WaveDb::open(path).map_err(|e| e.to_string())?;
        let config = WaveConfig::default();
        let profile = UserTasteProfile::from_recent(&db, unix_timestamp(), &config).map_err(|e| e.to_string())?;
        Ok(Self { db, config, profile })
    }

    fn store(&self, track: &TrackMetadata) -> Result<()> {
        validate_track(track)?;
        if !self.db.contains_track(&track.id).map_err(|e| e.to_string())? {
            self.db.upsert_track(track, vectorize(track, &[], 0)).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Store a discovered track; true only if it was not in the library yet.
    fn store_new(&self, track: &TrackMetadata) -> Result<bool> {
        validate_track(track)?;
        if self.db.contains_track(&track.id).map_err(|e| e.to_string())? { return Ok(false); }
        self.db.upsert_track(track, vectorize(track, &[], 0)).map_err(|e| e.to_string())?;
        Ok(true)
    }

    fn batch(&mut self, excluded: &HashSet<String>) -> Result<Vec<TrackMetadata>> {
        self.profile = UserTasteProfile::from_recent(&self.db, unix_timestamp(), &self.config).map_err(|e| e.to_string())?;
        // Reserve slots for never-played discoveries, or the taste profile keeps
        // ranking familiar tracks first and new ones never reach the queue.
        next_wave_mix(&self.db, 12, unix_timestamp(), &self.config, excluded, FRESH_RATIO)
            .map(|batch| batch.into_iter().map(|s| s.track).collect()).map_err(|e| e.to_string())
    }
}

struct Resolved {
    generation: u64,
    track: TrackMetadata,
    stream: Result<StreamInfo>,
    next: bool,
}

struct Worker {
    app: AppHandle,
    providers: Arc<Providers>,
    snapshot: Arc<RwLock<Snapshot>>,
    state: Snapshot,
    audio: AudioEngine,
    wave: WaveEngine,
    events: broadcast::Receiver<PlayerEvent>,
    queue: VecDeque<TrackMetadata>,
    search_results: Vec<TrackMetadata>,
    generation: u64,
    resolving: bool,
    resolution: Option<tokio::task::JoinHandle<Resolved>>,
    resolution_reply: PlaybackReply,
    failed: HashSet<String>,
    started: HashSet<String>,
    awaiting_start: bool,
    pending_play: Option<String>,
    pending_prepared: bool,
    /// The FFmpeg backend is active, so local Opus/WebM files are playable.
    ffmpeg: bool,
    discovery: Option<tokio::task::JoinHandle<Discovered>>,
    /// When the last discovery added nothing new; throttles retries while offline.
    discovery_empty_at: Option<Instant>,
    /// A Wave request on an empty library waits here for the discovery result.
    wave_reply: PlaybackReply<Vec<TrackMetadata>>,
}

impl Worker {
    fn new(app: AppHandle, providers: Arc<Providers>, snapshot: Arc<RwLock<Snapshot>>, path: PathBuf,
        ffmpeg: Option<PathBuf>) -> Result<Self> {
        let wave = WaveEngine::new(path)?;
        let has_ffmpeg = ffmpeg.is_some();
        let audio = match ffmpeg {
            Some(executable) => AudioEngine::with_decoder(Arc::new(FfmpegDecoder::new(executable))),
            None => AudioEngine::new(),
        }.map_err(|e| e.to_string())?;
        let events = audio.subscribe();
        Ok(Self { app, providers, snapshot, audio, wave, events, queue: VecDeque::new(), search_results: Vec::new(), generation: 0,
            resolving: false, resolution: None, resolution_reply: PlaybackReply::default(), failed: HashSet::new(), started: HashSet::new(),
            awaiting_start: false, pending_play: None, pending_prepared: false,
            ffmpeg: has_ffmpeg, discovery: None, discovery_empty_at: None, wave_reply: PlaybackReply::default(),
            state: Snapshot { volume: 1.0, preload: "idle".into(), ..Default::default() } })
    }

    /// Tracks the wave must not pick again right now.
    fn excluded(&self) -> HashSet<String> {
        let mut excluded = self.failed.clone();
        excluded.extend(self.queue.iter().map(|t| t.id.clone()));
        if let Some(t) = &self.state.track { excluded.insert(t.id.clone()); }
        if let Some(t) = &self.state.next_track { excluded.insert(t.id.clone()); }
        excluded
    }

    /// Start one online discovery when the unplayed library runs low. `force`
    /// skips the cooldown for an explicit user request.
    fn maybe_discover(&mut self, force: bool) {
        if !self.state.wave || self.discovery.is_some() { return; }
        // A failed count must not block discovery: treat it as an empty library.
        let unplayed = self.wave.db.unplayed_count(&self.excluded()).unwrap_or(0);
        let last_empty = if force { None } else { self.discovery_empty_at };
        if !should_discover(true, false, unplayed, last_empty, Instant::now()) { return; }
        // DB reads stay on this thread; the task only gets owned seeds.
        let mut seeds: Vec<TrackMetadata> = self.state.track.iter().cloned().collect();
        for seed in self.wave.db.seed_tracks(5).unwrap_or_default() {
            if !seeds.iter().any(|s| s.id == seed.id) { seeds.push(seed); }
        }
        let providers = self.providers.clone();
        self.discovery = Some(tokio::spawn(async move { discover(&providers, &seeds).await }));
        self.state.discovering = true;
    }

    fn discovered(&mut self, discovered: Discovered) {
        self.discovery = None;
        self.state.discovering = false;
        // Invalid provider rows are skipped, not fatal: the rest are still new music.
        let added = discovered.tracks.iter().filter(|t| self.wave.store_new(t).unwrap_or(false)).count();
        self.discovery_empty_at = if added == 0 { Some(Instant::now()) } else { None };
        let mut result = self.refill();
        let batch: Vec<TrackMetadata> = self.queue.iter().cloned().collect();
        if result.is_ok() && self.state.wave && self.state.track.is_none() {
            result = self.advance(false);
        }
        let idle = self.queue.is_empty() && self.state.track.is_none() && !self.resolving;
        if self.wave_reply.is_pending() && result.is_ok() && batch.is_empty() && idle && self.discovery.is_some() {
            // Refill already started a follow-up discovery; its result answers the request.
        } else if self.wave_reply.is_pending() {
            let reply = match result {
                Err(error) => Err(error),
                Ok(()) if batch.is_empty() && idle => Err(match discovered.error {
                    Some(error) => format!("{EMPTY_WAVE} ({error})"),
                    None => EMPTY_WAVE.into(),
                }),
                Ok(()) => Ok(batch),
            };
            if reply.is_err() { self.state.wave = false; }
            self.wave_reply.finish(reply);
        } else if let Err(error) = result {
            self.error(error);
        } else if added == 0 && idle && self.state.wave {
            // Background failures matter only once the wave has nothing left to play.
            if let Some(error) = discovered.error { self.error(error); }
        }
        self.publish();
    }

    fn publish(&mut self) {
        self.state.revision += 1;
        self.state.queue = self.queue.iter().cloned().collect();
        if let Ok(mut snapshot) = self.snapshot.write() { *snapshot = self.state.clone(); }
        let _ = self.app.emit("player-event", UiEvent::State { state: self.state.clone() });
    }

    fn error(&mut self, message: String) {
        self.state.error = Some(message.clone());
        let _ = self.app.emit("player-event", UiEvent::Error { message });
        self.publish();
    }

    fn refill(&mut self) -> Result<()> {
        if self.state.wave && self.queue.len() < 4 {
            let excluded = self.excluded();
            self.queue.extend(self.wave.batch(&excluded)?);
        }
        // Runs on start, every TrackStarted and after each refill; no-op outside the wave.
        self.maybe_discover(false);
        Ok(())
    }

    fn resolve(&mut self, track: TrackMetadata, next: bool, reply: Option<Reply<()>>) {
        self.resolving = true;
        if next {
            self.state.next_track = Some(track.clone());
            self.state.preload = "resolving".into();
        }
        self.resolution_reply = PlaybackReply(reply);
        let generation = self.generation;
        // A downloaded file needs no network and never expires.
        let local = self.wave.db.download_path(&track.id).ok().flatten()
            .and_then(|path| local_stream(&path, self.ffmpeg));
        if let Some(stream) = local {
            self.resolved(Resolved { generation, track, stream: Ok(stream), next });
            return;
        }
        let providers = self.providers.clone();
        self.resolution = Some(tokio::spawn(async move {
            let stream = providers.resolve(&track).await;
            Resolved { generation, track, stream, next }
        }));
        self.publish();
    }

    fn advance(&mut self, next: bool) -> Result<()> {
        if self.resolving || self.awaiting_start || self.state.next_track.is_some() { return Ok(()); }
        self.refill()?;
        if let Some(track) = self.queue.pop_front() { self.resolve(track, next, None); }
        Ok(())
    }

    fn reset_resolution(&mut self) {
        self.generation += 1;
        if let Some(task) = self.resolution.take() { task.abort(); }
        self.resolution_reply.finish(Err("Playback request superseded".into()));
        if self.state.preload == "resolving" {
            self.state.next_track = None;
            self.state.preload = "idle".into();
        }
        self.resolving = false;
    }

    async fn run(&mut self, mut requests: mpsc::Receiver<Request>) {
        self.publish();
        loop {
            tokio::select! {
                request = requests.recv() => match request {
                    Some(request) => self.request(request),
                    None => break,
                },
                result = async { self.resolution.as_mut().unwrap().await }, if self.resolution.is_some() => {
                    self.resolution = None;
                    match result {
                        Ok(resolved) => self.resolved(resolved),
                        Err(error) => {
                            self.resolving = false;
                            if self.state.preload == "resolving" {
                                self.state.next_track = None;
                                self.state.preload = "idle".into();
                            }
                            let reason = if error.is_panic() {
                                panic_category(error.into_panic().as_ref())
                            } else { "task cancelled" };
                            let message = format!("Stream resolver task failed ({reason}); playback worker is still running");
                            self.resolution_reply.finish(Err(message.clone()));
                            self.error(message);
                        }
                    }
                },
                result = async { self.discovery.as_mut().unwrap().await }, if self.discovery.is_some() => {
                    self.discovery = None;
                    self.discovered(result.unwrap_or_else(|error| {
                        let reason = if error.is_panic() { panic_category(error.into_panic().as_ref()) } else { "task cancelled" };
                        Discovered { tracks: Vec::new(), error: Some(format!("Discovery task failed ({reason})")) }
                    }));
                },
                event = self.events.recv() => match event {
                    Ok(event) => self.event(event),
                    Err(broadcast::error::RecvError::Lagged(n)) => self.error(format!("Lost {n} audio events; some listening statistics may be missing")),
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
        if let Some(task) = self.discovery.take() { task.abort(); }
        self.reset_resolution();
    }

    fn request(&mut self, request: Request) {
        match request {
            Request::Store(tracks, reply) => {
                let result = tracks.iter().try_for_each(|t| self.wave.store(t));
                if result.is_ok() { self.search_results = tracks.into_iter().take(100).collect(); }
                let _ = reply.send(result);
            }
            Request::Play(track, reply) => {
                if let Err(error) = self.wave.store(&track) { let _ = reply.send(Err(error)); return; }
                self.reset_resolution();
                self.queue.clear();
                if let Some(index) = self.search_results.iter().position(|t| t.id == track.id) {
                    self.queue.extend(self.search_results.iter().skip(index + 1).cloned());
                }
                self.failed.clear();
                self.state.wave = false;
                self.state.error = None;
                self.wave_reply.finish(Err("Playback request superseded".into()));
                // An already submitted next track remains visible until Play clears it.
                self.resolve(track, false, Some(reply));
            }
            Request::Wave(reply) => {
                self.failed.clear();
                if !self.state.wave { self.queue.clear(); }
                self.state.wave = true;
                self.state.error = None;
                self.wave_reply.finish(Err("Playback request superseded".into()));
                if let Err(error) = self.refill() {
                    self.state.wave = false;
                    self.publish();
                    let _ = reply.send(Err(error));
                    return;
                }
                if self.queue.is_empty() && self.state.track.is_none() {
                    // Empty library: wait for an online discovery (Deezer chart
                    // without seeds); `discovered` replies and starts playback.
                    self.maybe_discover(true);
                    if self.discovery.is_some() {
                        self.wave_reply = PlaybackReply(Some(reply));
                    } else {
                        self.state.wave = false;
                        let _ = reply.send(Err(EMPTY_WAVE.into()));
                    }
                    self.publish();
                    return;
                }
                let batch = self.queue.iter().cloned().collect();
                let result = if self.state.track.is_none() { self.advance(false).map(|()| batch) } else { Ok(batch) };
                if result.is_err() { self.state.wave = false; }
                self.publish();
                let _ = reply.send(result);
            }
            Request::Control(action, value, reply) => {
                let result = self.control(&action, value);
                self.publish();
                let _ = reply.send(result);
            }
            Request::PrepareDownload(track, reply) => {
                let result = self.wave.store(&track).and_then(|()| {
                    let db = &self.wave.db;
                    match db.download_path(&track.id).map_err(|e| e.to_string())? {
                        Some(path) if path.is_file() => Err("Already downloaded".into()),
                        // The user deleted the file outside the app: download it again.
                        Some(_) => db.remove_download(&track.id).map(|_| ()).map_err(|e| e.to_string()),
                        None => Ok(()),
                    }
                });
                let _ = reply.send(result);
            }
            Request::SetDownload(track_id, path, bytes, reply) => {
                let _ = reply.send(self.wave.db.set_download(&track_id, &path, bytes).map_err(|e| e.to_string()));
            }
            Request::RemoveDownload(track_id, reply) => {
                let _ = reply.send(self.wave.db.remove_download(&track_id).map_err(|e| e.to_string()));
            }
            Request::Downloads(reply) => {
                let result = self.wave.db.downloads().map_err(|e| e.to_string()).map(|rows| {
                    let (present, missing): (Vec<_>, Vec<_>) = rows.into_iter().partition(|(_, path, _)| path.is_file());
                    for (track, _, _) in missing { let _ = self.wave.db.remove_download(&track.id); }
                    present.into_iter().map(|(track, path, bytes)| DownloadItem { track, path: path.display().to_string(), bytes }).collect()
                });
                let _ = reply.send(result);
            }
        }
    }

    fn control(&mut self, action: &str, value: Option<f64>) -> Result<()> {
        let command = match action.to_ascii_lowercase().as_str() {
            "pause" => PlayerCommand::Pause,
            "resume" => PlayerCommand::Resume,
            "skip" => PlayerCommand::Skip,
            "seek" => {
                let ms = finite_value(value, 0.0, self.state.duration_ms as f64)?;
                PlayerCommand::Seek(Duration::from_millis(ms as u64))
            }
            "volume" => {
                // Sent many times per second while dragging. A full engine queue
                // drops this value silently: the next tick supersedes it.
                let volume = finite_value(value, 0.0, 1.0)? as f32;
                let _ = self.audio.send(PlayerCommand::SetVolume(volume));
                self.state.volume = volume;
                return Ok(());
            }
            _ => return Err("Unknown action: pause, resume, skip, seek, volume expected".into()),
        };
        self.audio.send(command.clone()).map_err(|e| e.to_string())?;
        match command {
            PlayerCommand::Pause => self.state.paused = true,
            PlayerCommand::Resume => self.state.paused = false,
            PlayerCommand::Skip => {
                // Skip must work even while paused; the engine consumes controls on its live source.
                self.audio.send(PlayerCommand::Resume).map_err(|e| e.to_string())?;
                self.state.paused = false;
            }
            _ => {}
        }
        Ok(())
    }

    fn resolved(&mut self, resolved: Resolved) {
        if resolved.generation != self.generation {
            return;
        }
        self.resolving = false;
        self.resolution = None;
        let result = resolved.stream.and_then(|stream| {
            let command = if resolved.next { PlayerCommand::QueueNext(resolved.track.clone(), stream) }
                else { PlayerCommand::Play(resolved.track.clone(), stream) };
            self.audio.send(command).map_err(|e| e.to_string())
        });
        if result.is_ok() {
            if resolved.next {
                // The previous track may have finished while the URL was resolving.
                self.state.next_track = Some(resolved.track.clone());
                self.state.preload = "buffering".into();
            }
            else {
                self.state.next_track = None;
                self.state.preload = "idle".into();
                self.awaiting_start = true;
                self.pending_play = Some(resolved.track.id.clone());
                self.pending_prepared = false;
                // Play resumes the engine. Later Pause requests must not be
                // overwritten by an asynchronously delivered TrackStarted.
                self.state.paused = false;
            }
        } else {
            self.failed.insert(resolved.track.id);
            if resolved.next { self.state.next_track = None; self.state.preload = "idle".into(); }
            if let Err(message) = &result { self.error(message.clone()); }
        }
        let failed = result.is_err();
        self.resolution_reply.finish(result);
        if failed && (self.state.wave || !self.queue.is_empty()) {
            if let Err(e) = self.advance(self.state.track.is_some()) { self.error(e); }
        }
        self.publish();
    }

    fn event(&mut self, event: PlayerEvent) {
        match event {
            PlayerEvent::TrackStarted { track } => {
                self.started.insert(track.id.clone());
                if self.pending_play.as_ref().is_some_and(|id| id == &track.id) {
                    self.awaiting_start = false;
                    self.pending_play = None;
                }
                if self.state.next_track.as_ref().is_some_and(|t| t.id == track.id) {
                    self.state.next_track = None;
                    self.state.preload = "idle".into();
                }
                self.state.duration_ms = track.duration_ms;
                self.state.track = Some(track);
                self.state.position_ms = 0;
                self.state.error = None;
                if let Err(e) = self.refill() { self.error(e); }
                if should_preload(0, self.state.duration_ms) {
                    if let Err(e) = self.advance(true) { self.error(e); }
                }
            }
            PlayerEvent::Progress { track_id, position_ms, duration_ms } => {
                if !self.state.track.as_ref().is_some_and(|t| t.id == track_id) { return; }
                self.state.position_ms = position_ms;
                self.state.duration_ms = duration_ms;
                if should_preload(position_ms, duration_ms) {
                    if let Err(e) = self.advance(true) { self.error(e); }
                }
            }
            PlayerEvent::PreloadTriggered { track_id } => {
                if self.pending_play.as_ref().is_some_and(|id| id == &track_id) { self.pending_prepared = true; }
                if self.state.next_track.as_ref().is_some_and(|t| t.id == track_id) { self.state.preload = "buffering".into(); }
            }
            PlayerEvent::PreloadReady { track_id } => {
                if self.state.next_track.as_ref().is_some_and(|t| t.id == track_id) { self.state.preload = "ready".into(); }
            }
            PlayerEvent::GaplessTransition { from_track_id, to_track_id } => {
                let _ = self.app.emit("player-event", UiEvent::Gapless { from_track_id, to_track_id });
            }
            PlayerEvent::TrackEnded { track_id, completion_ratio, skipped_early } => {
                // Decode failures with zero audible samples are not a negative taste signal.
                if self.started.remove(&track_id) {
                    if let Err(e) = self.wave.db.record_interaction(&Interaction {
                        track_id: track_id.clone(), completion_ratio, skipped_early, liked: false, timestamp: unix_timestamp(),
                    }) { self.error(e.to_string()); }
                }
                if self.state.track.as_ref().is_some_and(|t| t.id == track_id) {
                    self.state.track = None;
                    self.state.position_ms = 0;
                }
            }
            PlayerEvent::QueueFinished => {
                // Ignore an old idle notification preceding our newly submitted Play.
                if self.awaiting_start && !self.pending_prepared { return; }
                self.awaiting_start = false;
                self.pending_play = None;
                self.state.track = None;
                self.state.duration_ms = 0;
                if !self.resolving {
                    self.state.next_track = None;
                    self.state.preload = "idle".into();
                }
                if let Err(e) = self.advance(false) { self.error(e); }
            }
            PlayerEvent::Error { track_id, message } => {
                if let Some(id) = track_id {
                    if self.state.next_track.as_ref().is_some_and(|t| t.id == id) {
                        self.state.preload = "buffering".into();
                    }
                    self.failed.insert(id);
                }
                self.error(message);
            }
        }
        self.publish();
    }
}

fn should_preload(position_ms: u64, duration_ms: u64) -> bool {
    duration_ms > 0 && duration_ms.saturating_sub(position_ms) <= 10_000
}

fn finite_value(value: Option<f64>, min: f64, max: f64) -> Result<f64> {
    value.filter(|v| v.is_finite() && *v >= min && *v <= max)
        .ok_or_else(|| format!("Value must be finite and between {min} and {max}"))
}

fn validate_track(track: &TrackMetadata) -> Result<()> {
    if track.id.is_empty() || track.id.len() > 512 || track.title.trim().is_empty()
        || track.title.len() > 4096 || track.duration_ms == 0 || track.duration_ms > 86_400_000
        || track.artists.len() > 64 || track.artists.iter().any(|a| a.len() > 4096)
        || track.cover_url.as_ref().is_some_and(|s| s.len() > 4096)
        || track.album.as_ref().is_some_and(|s| s.len() > 4096)
        || track.isrc.as_ref().is_some_and(|s| s.len() > 128) {
        return Err("Invalid track metadata".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn search_state() -> (AppState, mpsc::Receiver<Request>) {
        let (tx, rx) = mpsc::channel(1);
        (AppState {
            providers: Arc::new(Providers {
                spotify: SpotifyResolver::new().unwrap(),
                youtube: YoutubeResolver::new_native().unwrap(),
                soundcloud: SoundCloudResolver::new_native().unwrap(),
                deezer: DeezerResolver::new().unwrap(),
                searches: Semaphore::new(2),
            }),
            tx,
            snapshot: Arc::new(RwLock::new(Snapshot::default())),
            downloads: Arc::new(Downloads::new(std::env::temp_dir())),
        }, rx)
    }

    fn named(id: &str, title: &str, artists: &[&str]) -> TrackMetadata {
        TrackMetadata { id: id.into(), title: title.into(), artists: artists.iter().map(|a| a.to_string()).collect(),
            album: None, duration_ms: 1000, cover_url: None, isrc: None }
    }

    #[test]
    fn spotify_auth_failures_also_fall_back() {
        use core_types::ResolverError::*;
        assert!(spotify_fallback(&Http { service: "Spotify", status: 401 }));
        assert!(spotify_fallback(&Transport { service: "Spotify", message: "dns".into() }));
        assert!(!spotify_fallback(&Http { service: "Spotify", status: 404 }));
        assert!(!spotify_fallback(&Parse { service: "Spotify", message: "x".into() }));
    }

    #[test]
    fn source_routing_separates_links_from_text() {
        for input in ["https://www.deezer.com/track/3135556", "deezer.com/us/track/1", "deezer:track:1",
            "link.deezer.com/s/abc", "3135556"] { assert!(is_deezer_reference(input), "{input}"); }
        for input in ["Blinding Lights", "1999 Prince", "2024", ""] {
            assert_eq!(is_deezer_reference(input), input == "2024", "{input}");
        }
        assert!(is_soundcloud_reference("https://soundcloud.com/a/b"));
        assert!(is_soundcloud_reference("soundcloud:track:1"));
        assert!(!is_soundcloud_reference("never gonna give you up"));
        assert_eq!(joined(&["Spotify: HTTP 451 (unavailable in this country)".into(), "Deezer: no match for x".into()]),
            "Spotify: HTTP 451 (unavailable in this country); Deezer: no match for x");
    }

    #[test]
    fn file_names_are_portable_and_bounded() {
        let track = named("youtube:video:dQw4w9WgXcQ", "Never: Gonna/Give\\You*Up?", &["Rick \"Astley\"", " ", "<A|B>"]);
        assert_eq!(safe_file_name(&track, "m4a"),
            "Rick _Astley_, _A_B_ - Never_ Gonna_Give_You_Up_ [youtube-video-dQw4w9WgXcQ].m4a");
        let dots = named("deezer:track:1", " ..hidden\u{0007}name.. ", &[]);
        assert_eq!(safe_file_name(&dots, "mp3"), "hidden_name [deezer-track-1].mp3");
        assert_eq!(safe_file_name(&named("x:1", "...", &[]), "mp3"), "track [x-1].mp3");
        let long = named("soundcloud:track:9", &"я".repeat(400), &["Artist"]);
        let name = safe_file_name(&long, "opus");
        assert!(name.len() <= 150 + 48 + 10, "{}", name.len());
        assert!(name.ends_with(" [soundcloud-track-9].opus"));
        assert!(!name.contains(['/', '\\', ':']));
        // Cut exactly at a char boundary, then trailing spaces removed.
        assert_eq!(file_component("ab cd", 3), "ab");
        assert_eq!(file_component("ééé", 5), "éé");
    }

    #[test]
    fn local_codec_follows_extension() {
        assert_eq!(codec_for_extension("mp3"), Some(AudioCodec::Mp3));
        assert_eq!(codec_for_extension("M4A"), Some(AudioCodec::Aac));
        assert_eq!(codec_for_extension("mp4"), Some(AudioCodec::Aac));
        for ext in ["webm", "opus", "ogg"] { assert_eq!(codec_for_extension(ext), Some(AudioCodec::Opus)); }
        assert_eq!(codec_for_extension("part"), None);
    }

    #[test]
    fn local_stream_requires_existing_absolute_playable_file() {
        let fixture = FfmpegFixture::new();
        let mp3 = fixture.0.join("a.mp3");
        let opus = fixture.0.join("a.opus");
        std::fs::write(&mp3, b"x").unwrap();
        std::fs::write(&opus, b"x").unwrap();
        let stream = local_stream(&mp3, false).unwrap();
        assert_eq!(stream.source, StreamSource::Local);
        assert_eq!(stream.codec, AudioCodec::Mp3);
        assert_eq!(stream.stream_url, mp3.to_str().unwrap());
        assert!(!stream.hls && stream.expires_at.is_none());
        // Native decoders cannot play Opus: fall back to network resolution.
        assert!(local_stream(&opus, false).is_none());
        assert_eq!(local_stream(&opus, true).unwrap().codec, AudioCodec::Opus);
        assert!(local_stream(&fixture.0.join("missing.mp3"), true).is_none());
        assert!(local_stream(Path::new("relative.mp3"), true).is_none());
        assert!(local_stream(&fixture.0, true).is_none());
    }

    #[test]
    fn discovery_triggers_only_when_low_idle_and_not_cooling_down() {
        let now = Instant::now();
        assert!(should_discover(true, false, 0, None, now));
        assert!(should_discover(true, false, DISCOVERY_LOW_WATER - 1, None, now));
        assert!(!should_discover(true, false, DISCOVERY_LOW_WATER, None, now));
        assert!(!should_discover(false, false, 0, None, now));
        assert!(!should_discover(true, true, 0, None, now));
        let later = now + DISCOVERY_COOLDOWN;
        assert!(!should_discover(true, false, 0, Some(now), now + Duration::from_secs(1)));
        assert!(should_discover(true, false, 0, Some(now), later));
    }

    #[test]
    fn discoveries_interleave_providers_and_dedup() {
        let ids = |tracks: Vec<TrackMetadata>| tracks.into_iter().map(|t| t.id).collect::<Vec<_>>();
        let t = |id: &str| named(id, "T", &["A"]);
        let merged = merge_discoveries(vec![vec![t("d1"), t("d2"), t("d3")], vec![t("y1"), t("d1")], vec![], vec![t("s1")]]);
        assert_eq!(ids(merged), ["d1", "y1", "s1", "d2", "d3"]);
        assert!(merge_discoveries(vec![vec![], vec![]]).is_empty());
    }

    #[test]
    fn snapshot_and_download_events_match_the_ui_contract() {
        let state = serde_json::to_value(Snapshot { discovering: true, ..Default::default() }).unwrap();
        assert_eq!(state["discovering"], true);
        let event = serde_json::to_value(UiEvent::Download { track_id: "x".into(), status: "failed", done_bytes: 0,
            total_bytes: None, message: Some("Download failed: HTTP 403".into()) }).unwrap();
        assert_eq!(event, serde_json::json!({ "type": "download", "track_id": "x", "status": "failed",
            "done_bytes": 0, "total_bytes": null, "message": "Download failed: HTTP 403" }));
    }

    #[tokio::test]
    async fn wave_reply_superseded_is_explicit() {
        let (tx, rx) = oneshot::channel::<Result<Vec<TrackMetadata>>>();
        let mut reply = PlaybackReply(Some(tx));
        assert!(reply.is_pending());
        reply.finish(Err("Playback request superseded".into()));
        assert!(!reply.is_pending());
        assert_eq!(rx.await.unwrap().unwrap_err(), "Playback request superseded");
    }

    #[test]
    fn active_download_and_part_file_clean_up_on_drop() {
        let fixture = FfmpegFixture::new();
        let downloads = Arc::new(Downloads::new(fixture.0.clone()));
        assert!(downloads.active().insert("a".into()));
        drop(ActiveDownload { downloads: downloads.clone(), id: "a".into() });
        assert!(downloads.active().is_empty());
        let path = fixture.0.join("x.m4a.part");
        std::fs::write(&path, b"x").unwrap();
        drop(PartFile { path: path.clone(), keep: false });
        assert!(!path.exists());
        std::fs::write(&path, b"x").unwrap();
        drop(PartFile { path: path.clone(), keep: true });
        assert!(path.exists());
    }

    fn search_track() -> TrackMetadata {
        TrackMetadata { id: "spotify:track:test".into(), title: "Test".into(),
            artists: vec!["Artist".into()], album: None, duration_ms: 1000,
            cover_url: None, isrc: None }
    }

    #[test]
    fn spotify_region_block_is_explicit_and_falls_back() {
        use core_types::ResolverError::*;
        assert_eq!(spotify_error(Http { service: "Spotify", status: 451 }), "Spotify: HTTP 451 (unavailable in this country)");
        assert_eq!(spotify_error(Http { service: "Spotify", status: 404 }), "Spotify: HTTP 404");
        assert!(spotify_fallback(&Http { service: "Spotify", status: 451 }));
        assert!(spotify_fallback(&RateLimited { service: "Spotify", retry_after_secs: 60 }));
        assert!(!spotify_fallback(&NotFound { service: "Spotify", query: "x".into() }));
        assert!(!spotify_fallback(&InvalidInput("x".into())));
    }

    #[tokio::test]
    async fn search_times_out_while_store_is_queued_and_releases_permit() {
        let (state, mut requests) = search_state();
        let result = state.search_with_metadata(Duration::from_millis(20), async {
            Ok(vec![search_track()])
        }).await;
        assert_eq!(result.unwrap_err(), "Search timed out");
        let Request::Store(_, reply) = requests.try_recv().unwrap() else { panic!("expected Store") };
        assert!(reply.is_closed());
        assert_eq!(state.providers.searches.available_permits(), 2);
    }

    #[tokio::test]
    async fn search_uses_one_deadline_for_metadata_and_store_reply() {
        let (state, mut requests) = search_state();
        let start = tokio::time::Instant::now();
        let search = state.search_with_metadata(Duration::from_millis(100), async {
            tokio::time::sleep(Duration::from_millis(60)).await;
            Ok(vec![search_track()])
        });
        let actor = async {
            let Request::Store(_, mut reply) = requests.recv().await.unwrap() else { panic!("expected Store") };
            // With a fresh storage budget this acknowledgement would succeed.
            tokio::select! {
                _ = reply.closed() => {},
                _ = tokio::time::sleep_until(start + Duration::from_millis(140)) => {
                    let _ = reply.send(Ok(()));
                }
            }
        };
        let (result, ()) = tokio::join!(search, actor);
        assert_eq!(result.unwrap_err(), "Search timed out");
        assert_eq!(state.providers.searches.available_permits(), 2);
    }

    #[tokio::test]
    async fn search_metadata_timeout_never_submits_store() {
        let (state, mut requests) = search_state();
        let result = state.search_with_metadata(Duration::from_millis(20), std::future::pending()).await;
        assert_eq!(result.unwrap_err(), "Search timed out");
        assert!(matches!(requests.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
        assert_eq!(state.providers.searches.available_permits(), 2);
    }

    #[tokio::test]
    async fn search_preserves_tracks_and_storage_errors() {
        let (state, mut requests) = search_state();
        for acknowledgement in [Ok(()), Err("storage failed".to_string())] {
            let search = state.search_with_metadata(Duration::from_secs(1), async { Ok(vec![search_track()]) });
            let actor = async {
                let Request::Store(tracks, reply) = requests.recv().await.unwrap() else { panic!("expected Store") };
                assert_eq!(tracks[0].id, "spotify:track:test");
                reply.send(acknowledgement.clone()).unwrap();
            };
            let (result, ()) = tokio::join!(search, actor);
            match acknowledgement {
                Ok(()) => {
                    let tracks = result.unwrap();
                    assert_eq!(tracks.len(), 1);
                    assert_eq!(tracks[0].id, "spotify:track:test");
                }
                Err(error) => assert_eq!(result.unwrap_err(), error),
            }
        }
    }

    #[tokio::test]
    async fn search_preserves_provider_limit_and_mailbox_errors() {
        let (state, mut requests) = search_state();
        let budget = Duration::from_secs(1);
        let error = "Spotify: HTTP 429 (rate limited); retry in 60 seconds";
        assert_eq!(state.search_with_metadata(budget, async { Err(error.into()) }).await.unwrap_err(), error);
        assert_eq!(state.search_with_metadata(budget, async { Ok(vec![search_track(); 2001]) }).await.unwrap_err(),
            "Playlist exceeds 2000 tracks");
        assert!(matches!(requests.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
        let permit = state.providers.searches.acquire_many(2).await.unwrap();
        assert_eq!(state.search_with_metadata(budget, std::future::pending()).await.unwrap_err(), "Search busy; try again");
        drop(permit);
        let (reply, _) = oneshot::channel();
        state.send(Request::Store(vec![], reply)).await.unwrap();
        assert!(state.search_with_metadata(budget, async { Ok(vec![]) }).await.unwrap_err().starts_with("Player busy or unavailable:"));
        requests.recv().await.unwrap();
        let search = state.search_with_metadata(budget, async { Ok(vec![]) });
        let actor = async { drop(requests.recv().await.unwrap()); };
        let (result, ()) = tokio::join!(search, actor);
        assert_eq!(result.unwrap_err(), state.reply_error());
        drop(requests);
        assert!(state.search_with_metadata(budget, async { Ok(vec![]) }).await.unwrap_err().starts_with("Player busy or unavailable:"));
    }

    struct FfmpegFixture(PathBuf);

    impl FfmpegFixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!("wave-ffmpeg-{}-{}", std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
            std::fs::create_dir(&dir).unwrap();
            Self(dir)
        }

        fn executable(&self, dir: &str) -> PathBuf {
            let dir = self.0.join(dir);
            std::fs::create_dir(&dir).unwrap();
            let path = dir.join(if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" });
            // Deliberately not a real binary: discovery must never execute it.
            std::fs::write(&path, b"not an FFmpeg binary").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            path
        }
    }

    impl Drop for FfmpegFixture {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    #[test]
    fn ffmpeg_discovery_precedence() {
        let fixture = FfmpegFixture::new();
        let explicit = fixture.executable("explicit");
        let adjacent = fixture.executable("app");
        let first = fixture.executable("first");
        let second = fixture.executable("second");
        let app = adjacent.parent().unwrap().join("player");
        let path = std::env::join_paths([first.parent().unwrap(), second.parent().unwrap()]).unwrap();
        assert_eq!(ffmpeg_path(Some(explicit.clone().into_os_string()), Some(app.clone()), Some(path.clone()), false).unwrap(), Some(explicit));
        assert_eq!(ffmpeg_path(None, Some(app.clone()), Some(path.clone()), false).unwrap(), Some(adjacent.clone()));
        std::fs::remove_file(adjacent).unwrap();
        assert_eq!(ffmpeg_path(None, Some(app), Some(path.clone()), false).unwrap(), Some(first.clone()));
        std::fs::remove_file(first).unwrap();
        assert_eq!(ffmpeg_path(None, None, Some(path), false).unwrap(), Some(second));
    }

    #[test]
    fn ffmpeg_invalid_override_never_falls_back() {
        let fixture = FfmpegFixture::new();
        let adjacent = fixture.executable("app");
        for value in [PathBuf::new(), PathBuf::from("ffmpeg.exe"), fixture.0.clone(),
            fixture.0.join("missing.exe"), PathBuf::from(format!("{} -version", adjacent.display()))] {
            assert!(ffmpeg_path(Some(value.into_os_string()), Some(adjacent.clone()), None, false).is_err());
        }
    }

    #[test]
    fn ffmpeg_skips_relative_empty_and_invalid_candidates() {
        let fixture = FfmpegFixture::new();
        let invalid = fixture.executable("invalid");
        std::fs::remove_file(&invalid).unwrap();
        std::fs::create_dir(&invalid).unwrap();
        let valid = fixture.executable("valid");
        let path = std::env::join_paths([std::path::Path::new(""), std::path::Path::new("relative"),
            invalid.parent().unwrap(), valid.parent().unwrap()]).unwrap();
        assert_eq!(ffmpeg_path(None, Some(invalid), Some(path), false).unwrap(), Some(valid));
        assert_eq!(ffmpeg_path(None, Some("relative/player".into()), Some("".into()), false).unwrap(), None);
        assert_eq!(ffmpeg_path(None, None, None, false).unwrap(), None);
    }

    #[test]
    fn ffmpeg_mobile_stays_native_and_rejects_override() {
        let fixture = FfmpegFixture::new();
        let executable = fixture.executable("app");
        let path = std::env::join_paths([executable.parent().unwrap()]).unwrap();
        assert_eq!(ffmpeg_path(None, Some(executable.clone()), Some(path.clone()), true).unwrap(), None);
        assert_eq!(ffmpeg_path(Some(executable.clone().into_os_string()), Some(executable), Some(path), true).unwrap_err(),
            "WAVE_FFMPEG is supported only on desktop");
    }

    #[test]
    #[cfg(unix)]
    fn ffmpeg_requires_execute_permission() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = FfmpegFixture::new();
        let executable = fixture.executable("app");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o644)).unwrap();
        let path = std::env::join_paths([executable.parent().unwrap()]).unwrap();
        assert!(ffmpeg_path(Some(executable.clone().into_os_string()), None, None, false).is_err());
        assert_eq!(ffmpeg_path(None, Some(executable), Some(path), false).unwrap(), None);
    }

    #[tokio::test]
    async fn provider_panic_after_await_is_reported_without_its_payload() {
        let error = resolve_provider("SoundCloud", async {
            tokio::task::yield_now().await;
            panic!("index out of bounds: https://media.invalid/audio?token=secret");
        }).await.unwrap_err();
        assert!(error.contains("SoundCloud: resolver panic (index out of bounds)"));
        assert!(!error.contains("secret"));
        assert!(!error.contains("https://"));
    }

    #[tokio::test]
    async fn provider_errors_are_not_misreported_as_panics() {
        let error = resolve_provider("SoundCloud", async {
            Err(core_types::ResolverError::Unavailable {
                service: "SoundCloud", message: "no progressive MP3 stream".into(),
            })
        }).await.unwrap_err();
        assert_eq!(error, "SoundCloud: unavailable: no progressive MP3 stream");
    }

    #[test]
    fn unknown_panic_payload_is_not_exposed() {
        let payload = "private response with token=secret".to_string();
        assert_eq!(panic_category(&payload),
            "internal panic; console panic location needed (omit URLs and tokens)");
    }

    #[tokio::test]
    async fn cancelled_play_has_an_explicit_reply_not_a_closed_channel() {
        let (tx, rx) = oneshot::channel();
        let mut reply = PlaybackReply::<()>(Some(tx));
        reply.finish(Err("Playback request superseded".into()));
        drop(reply);
        assert_eq!(rx.await.unwrap(), Err("Playback request superseded".into()));
    }

    #[tokio::test]
    async fn dropped_play_reply_reports_cancellation() {
        let (tx, rx) = oneshot::channel();
        drop(PlaybackReply::<()>(Some(tx)));
        assert_eq!(rx.await.unwrap(), Err("Playback request cancelled".into()));
    }

    #[test]
    fn preload_window_is_last_ten_seconds() {
        assert!(!should_preload(89_999, 100_000));
        assert!(should_preload(90_000, 100_000));
        assert!(should_preload(0, 5_000));
        assert!(!should_preload(100, 0));
    }

    #[test]
    fn rejects_non_finite_and_out_of_range_controls() {
        for value in [None, Some(f64::NAN), Some(f64::INFINITY), Some(-0.1), Some(1.1)] {
            assert!(finite_value(value, 0.0, 1.0).is_err());
        }
        assert_eq!(finite_value(Some(0.5), 0.0, 1.0).unwrap(), 0.5);
    }
}
