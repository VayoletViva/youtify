use std::{collections::{HashSet, VecDeque}, path::PathBuf, sync::{Arc, RwLock}, time::Duration};
use audio_engine::{decoder::FfmpegDecoder, AudioEngine, PlayerCommand, PlayerEvent};
use core_types::{MetadataProvider, StreamInfo, StreamResolver, TrackMetadata};
use resolvers::{spotify::Spotify as SpotifyResolver, youtube::YouTube as YoutubeResolver,
    soundcloud::SoundCloud as SoundCloudResolver};
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::{broadcast, mpsc, oneshot, Semaphore};
use wave_engine::{features::vectorize, next_wave_batch, unix_timestamp, Interaction, UserTasteProfile, WaveConfig, WaveDb};

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
#[derive(Default)]
struct PlaybackReply(Option<Reply<()>>);

impl PlaybackReply {
    fn finish(&mut self, result: Result<()>) {
        if let Some(reply) = self.0.take() { let _ = reply.send(result); }
    }
}

impl Drop for PlaybackReply {
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
}

#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum UiEvent {
    State { state: Snapshot },
    Gapless { from_track_id: String, to_track_id: String },
    Error { message: String },
}

struct Providers {
    spotify: SpotifyResolver,
    youtube: YoutubeResolver,
    soundcloud: SoundCloudResolver,
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

/// Tauri requires Send + Sync state. AudioEngine's device handle is deliberately
/// NOT moved across threads: the actor owns it and SQLite on one dedicated thread.
/// The shared facade owns the actor's bounded mailbox, providers and UI snapshot.
pub struct AppState {
    providers: Arc<Providers>,
    tx: mpsc::Sender<Request>,
    pub snapshot: Arc<RwLock<Snapshot>>,
}

pub enum Request {
    Play(TrackMetadata, Reply<()>),
    Control(String, Option<f64>, Reply<()>),
    Wave(Reply<Vec<TrackMetadata>>),
    Store(Vec<TrackMetadata>, Reply<()>),
}

impl AppState {
    pub fn new(app: AppHandle, path: PathBuf) -> std::result::Result<Self, Box<dyn std::error::Error>> {
        // Read once so provider format selection and the decoder cannot disagree.
        let ffmpeg = ffmpeg_path(std::env::var_os("WAVE_FFMPEG"), std::env::current_exe().ok(),
            std::env::var_os("PATH"), cfg!(any(target_os = "android", target_os = "ios")))
            .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;
        let providers = Arc::new(Providers {
            spotify: SpotifyResolver::new()?,
            youtube: if ffmpeg.is_some() { YoutubeResolver::new()? } else { YoutubeResolver::new_native()? },
            soundcloud: if ffmpeg.is_some() { SoundCloudResolver::new()? } else { SoundCloudResolver::new_native()? },
            searches: Semaphore::new(2),
        });
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
        Ok(Self { providers, tx, snapshot })
    }

    pub async fn send(&self, request: Request) -> Result<()> {
        self.tx.try_send(request).map_err(|e| format!("Player busy or unavailable: {e}"))
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
            match source.to_ascii_lowercase().as_str() {
                "spotify" => self.providers.spotify.track(query).await.map(|t| vec![t]),
                "soundcloud" => self.providers.soundcloud.track(query).await.map(|t| vec![t]),
                "spotify-playlist" => self.providers.spotify.playlist(query).await,
                "soundcloud-playlist" => self.providers.soundcloud.playlist(query).await,
                _ => return Err("Source must be spotify, soundcloud, spotify-playlist or soundcloud-playlist; YouTube exposes streams only".into()),
            }.map_err(|e| e.to_string())
        }).await
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

    fn batch(&mut self, excluded: &HashSet<String>) -> Result<Vec<TrackMetadata>> {
        self.profile = UserTasteProfile::from_recent(&self.db, unix_timestamp(), &self.config).map_err(|e| e.to_string())?;
        next_wave_batch(&self.db, 12, unix_timestamp(), &self.config, excluded)
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
}

impl Worker {
    fn new(app: AppHandle, providers: Arc<Providers>, snapshot: Arc<RwLock<Snapshot>>, path: PathBuf,
        ffmpeg: Option<PathBuf>) -> Result<Self> {
        let wave = WaveEngine::new(path)?;
        let audio = match ffmpeg {
            Some(executable) => AudioEngine::with_decoder(Arc::new(FfmpegDecoder::new(executable))),
            None => AudioEngine::new(),
        }.map_err(|e| e.to_string())?;
        let events = audio.subscribe();
        Ok(Self { app, providers, snapshot, audio, wave, events, queue: VecDeque::new(), search_results: Vec::new(), generation: 0,
            resolving: false, resolution: None, resolution_reply: PlaybackReply::default(), failed: HashSet::new(), started: HashSet::new(),
            awaiting_start: false, pending_play: None, pending_prepared: false,
            state: Snapshot { volume: 1.0, preload: "idle".into(), ..Default::default() } })
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
        if !self.state.wave || self.queue.len() >= 4 { return Ok(()); }
        let mut excluded = self.failed.clone();
        excluded.extend(self.queue.iter().map(|t| t.id.clone()));
        if let Some(t) = &self.state.track { excluded.insert(t.id.clone()); }
        if let Some(t) = &self.state.next_track { excluded.insert(t.id.clone()); }
        self.queue.extend(self.wave.batch(&excluded)?);
        Ok(())
    }

    fn resolve(&mut self, track: TrackMetadata, next: bool, reply: Option<Reply<()>>) {
        self.resolving = true;
        if next {
            self.state.next_track = Some(track.clone());
            self.state.preload = "resolving".into();
        }
        let providers = self.providers.clone();
        self.resolution_reply = PlaybackReply(reply);
        let generation = self.generation;
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
                event = self.events.recv() => match event {
                    Ok(event) => self.event(event),
                    Err(broadcast::error::RecvError::Lagged(n)) => self.error(format!("Lost {n} audio events; some listening statistics may be missing")),
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
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
                // An already submitted next track remains visible until Play clears it.
                self.resolve(track, false, Some(reply));
            }
            Request::Wave(reply) => {
                self.failed.clear();
                if !self.state.wave { self.queue.clear(); }
                self.state.wave = true;
                self.state.error = None;
                let result = self.refill().and_then(|()| {
                    if self.queue.is_empty() && self.state.track.is_none() {
                        return Err("Wave library is empty. Search for tracks or import a playlist first.".into());
                    }
                    let batch = self.queue.iter().cloned().collect();
                    if self.state.track.is_none() { self.advance(false)?; }
                    Ok(batch)
                });
                if result.is_err() { self.state.wave = false; }
                self.publish();
                let _ = reply.send(result);
            }
            Request::Control(action, value, reply) => {
                let result = self.control(&action, value);
                self.publish();
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
            "volume" => PlayerCommand::SetVolume(finite_value(value, 0.0, 1.0)? as f32),
            _ => return Err("Unknown action: pause, resume, skip, seek, volume expected".into()),
        };
        self.audio.send(command.clone()).map_err(|e| e.to_string())?;
        match command {
            PlayerCommand::Pause => self.state.paused = true,
            PlayerCommand::Resume => self.state.paused = false,
            PlayerCommand::SetVolume(value) => self.state.volume = value,
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
                searches: Semaphore::new(2),
            }),
            tx,
            snapshot: Arc::new(RwLock::new(Snapshot::default())),
        }, rx)
    }

    fn search_track() -> TrackMetadata {
        TrackMetadata { id: "spotify:track:test".into(), title: "Test".into(),
            artists: vec!["Artist".into()], album: None, duration_ms: 1000,
            cover_url: None, isrc: None }
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
        let mut reply = PlaybackReply(Some(tx));
        reply.finish(Err("Playback request superseded".into()));
        drop(reply);
        assert_eq!(rx.await.unwrap(), Err("Playback request superseded".into()));
    }

    #[tokio::test]
    async fn dropped_play_reply_reports_cancellation() {
        let (tx, rx) = oneshot::channel();
        drop(PlaybackReply(Some(tx)));
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
