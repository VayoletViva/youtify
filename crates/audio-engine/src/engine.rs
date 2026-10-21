use crate::{decoder::{DecoderBackend, NativeDecoder}, gapless::{self, Control, Pipeline, Shared, Track},
    EngineError, PlayerCommand, PlayerEvent, Result};
use crossbeam_channel::{bounded, Sender};
use rodio::{OutputStream, OutputStreamBuilder, Sink};
use std::{collections::VecDeque, sync::{Arc, atomic::Ordering}, thread, time::Duration};
use tokio::{runtime::Handle, sync::broadcast};

/// Owns exactly one device stream and one sink for its entire lifetime.
/// Create inside a live Tokio runtime; keep that runtime alive until drop.
pub struct AudioEngine {
    commands: Sender<PlayerCommand>,
    shared: Arc<Shared>,
    events: broadcast::Sender<PlayerEvent>,
    manager: Option<thread::JoinHandle<()>>,
    _sink: Sink,
    _output: OutputStream,
}

impl AudioEngine {
    pub fn new() -> Result<Self> { Self::with_decoder(Arc::new(NativeDecoder::new()?)) }

    pub fn with_decoder(backend: Arc<dyn DecoderBackend>) -> Result<Self> {
        let runtime = Handle::try_current().map_err(|e| EngineError::Message(e.to_string()))?;
        let output = OutputStreamBuilder::open_default_stream().map_err(|e| EngineError::Message(e.to_string()))?;
        let channels = output.config().channel_count();
        let rate = output.config().sample_rate();
        let sink = Sink::connect_new(output.mixer());
        let (commands, receiver) = bounded(64);
        let (control_tx, control_rx) = bounded(64);
        let (events, _) = broadcast::channel(256);
        let shared = Arc::new(Shared::default());
        sink.append(Pipeline::new(control_rx, shared.clone(), events.clone(), channels, rate));
        let state = shared.clone();
        let telemetry = events.clone();
        let manager = thread::Builder::new().name("audio-control".into()).spawn(move || {
            let mut active: VecDeque<Arc<Track>> = VecDeque::new();
            let mut queue = VecDeque::new();
            let mut announced_idle = true;
            while !state.stopped.load(Ordering::Acquire) {
                active.retain(|track| !track.finished.load(Ordering::Acquire));
                match receiver.recv_timeout(Duration::from_millis(10)) {
                    Ok(PlayerCommand::Play(metadata, stream)) => {
                        queue.clear();
                        active.clear();
                        if !send_control(&control_tx, Control::Clear, &state) { break; }
                        state.paused.store(false, Ordering::Relaxed);
                        queue.push_back((metadata, stream));
                        announced_idle = false;
                    }
                    Ok(PlayerCommand::QueueNext(metadata, stream)) => {
                        if queue.len() >= 128 {
                            let _ = telemetry.send(PlayerEvent::Error { track_id: Some(metadata.id), message: "track queue is full (128)".into() });
                        } else { queue.push_back((metadata, stream)); announced_idle = false; }
                    }
                    Ok(PlayerCommand::Pause) => state.paused.store(true, Ordering::Relaxed),
                    Ok(PlayerCommand::Resume) => state.paused.store(false, Ordering::Relaxed),
                    Ok(PlayerCommand::SetVolume(value)) => {
                        if value.is_finite() { state.volume.store(value.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed); }
                    }
                    Ok(PlayerCommand::SetCrossfade(duration)) => {
                        state.crossfade_ms.store(duration.as_millis().min(10_000) as u64, Ordering::Relaxed);
                    }
                    Ok(PlayerCommand::Skip) => { if !send_control(&control_tx, Control::Skip, &state) { break; } }
                    Ok(PlayerCommand::Seek(position)) => { if !send_control(&control_tx, Control::Seek(position), &state) { break; } }
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                }
                // Only current + next have a network connection and decoder.
                while active.len() < 2 {
                    let Some((metadata, stream)) = queue.pop_front() else { break; };
                    let _ = telemetry.send(PlayerEvent::PreloadTriggered { track_id: metadata.id.clone() });
                    let (track, buffer) = gapless::prepare(metadata, stream, backend.clone(), runtime.clone(),
                        channels, rate, state.clone(), telemetry.clone());
                    active.push_back(track);
                    if !send_control(&control_tx, Control::Append(buffer), &state) { break; }
                }
                if active.is_empty() && queue.is_empty() && !announced_idle {
                    let _ = telemetry.send(PlayerEvent::QueueFinished);
                    announced_idle = true;
                }
            }
            for track in active { track.cancelled.store(true, Ordering::Release); }
        })?;
        Ok(Self { commands, shared, events, manager: Some(manager), _sink: sink, _output: output })
    }

    /// Bounded, non-blocking command submission. Retry on queue-full errors.
    pub fn send(&self, command: PlayerCommand) -> Result<()> {
        self.commands.try_send(command).map_err(|e| EngineError::Message(e.to_string()))
    }

    /// Subscribe before Play. A slow subscriber receives `Lagged`, never
    /// forces unbounded RAM growth or stalls the audio callback.
    pub fn subscribe(&self) -> broadcast::Receiver<PlayerEvent> { self.events.subscribe() }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Release);
        // Unblock control sends even if the device stopped calling its source.
        self._sink.stop();
        if let Some(manager) = self.manager.take() { let _ = manager.join(); }
    }
}

fn send_control(sender: &Sender<Control>, mut command: Control, shared: &Shared) -> bool {
    while !shared.stopped.load(Ordering::Acquire) {
        match sender.send_timeout(command, Duration::from_millis(20)) {
            Ok(()) => return true,
            Err(crossbeam_channel::SendTimeoutError::Timeout(value)) => command = value,
            Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => return false,
        }
    }
    false
}