use crate::{decoder::DecoderBackend, PlayerEvent};
use core_types::{StreamInfo, TrackMetadata};
use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError};
use rodio::{source::UniformSourceIterator, Source};
use std::{collections::VecDeque, sync::{Arc, atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering}},
    thread, time::Duration};
use tokio::{runtime::Handle, sync::broadcast};

const BLOCK_FRAMES: usize = 1024;
const BLOCKS: usize = 96;
/// Volume changes glide over this long; the UI sends SetVolume continuously
/// while the slider moves, and a step per control read would click.
const VOLUME_RAMP_MS: u64 = 20;

/// Linear per-frame gain ramp. Retargeting restarts the ramp from the gain
/// currently applied, so repeated updates never jump.
#[derive(Debug, Clone, Copy)]
struct Ramp { gain: f32, target: f32, step: f32, frames: u32 }

impl Ramp {
    fn new(gain: f32) -> Self { Self { gain, target: gain, step: 0.0, frames: 0 } }

    fn retarget(&mut self, target: f32, frames: u32) {
        if target == self.target { return; }
        self.target = target;
        if frames == 0 { self.gain = target; self.frames = 0; return; }
        self.frames = frames;
        self.step = (target - self.gain) / frames as f32;
    }

    /// Gain for the next frame (all its channels).
    fn advance(&mut self) -> f32 {
        if self.frames > 0 {
            self.frames -= 1;
            // Land exactly on the target; no float drift from summed steps.
            self.gain = if self.frames == 0 { self.target } else { self.gain + self.step };
        }
        self.gain
    }
}

pub(crate) struct Shared {
    pub stopped: AtomicBool,
    pub paused: AtomicBool,
    pub volume: AtomicU32,
    pub crossfade_ms: AtomicU64,
}
impl Default for Shared {
    fn default() -> Self {
        Self { stopped: AtomicBool::new(false), paused: AtomicBool::new(false),
            volume: AtomicU32::new(1.0f32.to_bits()), crossfade_ms: AtomicU64::new(0) }
    }
}

pub(crate) struct Track {
    metadata: TrackMetadata,
    pub finished: AtomicBool,
    pub cancelled: Arc<AtomicBool>,
    duration_ms: AtomicU64,
}

#[derive(Debug)]
enum Packet { Samples(u64, Vec<f32>), End(u64) }

/// Bounded decoded PCM plus the live decoder's control channel. Constructed
/// in the background before the current source reaches its last sample.
pub(crate) struct NextTrackBuffer {
    track: Arc<Track>,
    packets: Receiver<Packet>,
    recycle: Sender<Vec<f32>>,
    seeks: Sender<(u64, Duration)>,
    samples: Vec<f32>,
    offset: usize,
    epoch: u64,
    position: u64,
    listened: u64,
    started: bool,
    last_progress: u64,
}

enum Poll { Sample(f32), Pending, End }

impl NextTrackBuffer {
    fn poll(&mut self) -> Poll {
        if self.offset < self.samples.len() {
            let value = self.samples[self.offset];
            self.offset += 1;
            return Poll::Sample(value);
        }
        if !self.samples.is_empty() {
            let _ = self.recycle.try_send(std::mem::take(&mut self.samples));
        }
        // Bounded stale-generation draining after seek; never waits for I/O.
        for _ in 0..=BLOCKS {
            match self.packets.try_recv() {
                Ok(Packet::Samples(epoch, samples)) if epoch == self.epoch => {
                    self.samples = samples;
                    self.offset = 0;
                    if let Some(&sample) = self.samples.first() { self.offset = 1; return Poll::Sample(sample); }
                }
                Ok(Packet::Samples(_, samples)) => { let _ = self.recycle.try_send(samples); }
                Ok(Packet::End(epoch)) if epoch == self.epoch => return Poll::End,
                Ok(Packet::End(_)) => {}
                Err(TryRecvError::Empty) => return Poll::Pending,
                Err(TryRecvError::Disconnected) => return Poll::End,
            }
        }
        Poll::Pending
    }

    fn consumed(&mut self, events: &broadcast::Sender<PlayerEvent>, samples_per_second: u64) {
        if !self.started {
            self.started = true;
            let _ = events.send(PlayerEvent::TrackStarted { track: self.track.metadata.clone() });
        }
        self.position += 1;
        self.listened += 1;
        let position_ms = self.position.saturating_mul(1000) / samples_per_second;
        if position_ms / 250 != self.last_progress / 250 {
            self.last_progress = position_ms;
            let _ = events.send(PlayerEvent::Progress { track_id: self.track.metadata.id.clone(), position_ms,
                duration_ms: self.track.duration_ms.load(Ordering::Relaxed) });
        }
    }

    fn end(&mut self, events: &broadcast::Sender<PlayerEvent>, samples_per_second: u64, skipped: bool) {
        if self.track.finished.load(Ordering::Acquire) { return; }
        let duration_ms = self.track.duration_ms.load(Ordering::Relaxed);
        let ratio = if duration_ms == 0 {
            if !skipped && self.listened > 0 { 1.0 } else { 0.0 }
        } else { (self.listened as f64 * 1000.0 / samples_per_second as f64 / duration_ms as f64).clamp(0.0, 1.0) as f32 };
        let _ = events.send(PlayerEvent::TrackEnded { track_id: self.track.metadata.id.clone(),
            completion_ratio: ratio, skipped_early: skipped });
        self.track.cancelled.store(true, Ordering::Release);
        // Publish only after TrackEnded has been sent, preserving event order.
        self.track.finished.store(true, Ordering::Release);
    }

    fn seek(&mut self, position: Duration, samples_per_second: u64) -> bool {
        let duration = self.track.duration_ms.load(Ordering::Relaxed);
        let position = if duration > 0 { position.min(Duration::from_millis(duration)) } else { position };
        let epoch = self.epoch.wrapping_add(1);
        if self.seeks.try_send((epoch, position)).is_err() { return false; }
        self.epoch = epoch;
        self.offset = self.samples.len();
        self.position = (position.as_secs_f64() * samples_per_second as f64) as u64;
        self.last_progress = position.as_millis().min(u64::MAX as u128) as u64;
        true
    }
}

impl Drop for NextTrackBuffer {
    fn drop(&mut self) { self.track.cancelled.store(true, Ordering::Release); }
}

pub(crate) fn prepare(metadata: TrackMetadata, stream: StreamInfo, backend: Arc<dyn DecoderBackend>,
    runtime: Handle, channels: u16, rate: u32, shared: Arc<Shared>, events: broadcast::Sender<PlayerEvent>)
    -> (Arc<Track>, NextTrackBuffer) {
    let track = Arc::new(Track { duration_ms: AtomicU64::new(metadata.duration_ms), metadata,
        finished: AtomicBool::new(false), cancelled: Arc::new(AtomicBool::new(false)) });
    let (sender, packets) = bounded(BLOCKS);
    let (recycle, recycled) = bounded(BLOCKS + 2);
    let (seeks, seek_rx) = bounded(8);
    let worker_track = track.clone();
    let worker_events = events.clone();
    let spawn = thread::Builder::new().name("audio-decode".into()).spawn(move || {
        let track = worker_track;
        let stopped = || track.cancelled.load(Ordering::Acquire) || shared.stopped.load(Ordering::Acquire);
        let mut epoch = 0;
        let mut position = Duration::ZERO;
        let mut announced_ready = false;
        'restart: loop {
            if stopped() { break; }
            let source = match backend.open(&stream, position, &runtime, track.cancelled.clone()) {
                Ok(source) => source,
                Err(error) => {
                    let _ = worker_events.send(PlayerEvent::Error { track_id: Some(track.metadata.id.clone()), message: error.to_string() });
                    let _ = sender.send_timeout(Packet::End(epoch), Duration::from_millis(50));
                    break;
                }
            };
            // Only a source opened at zero reports the whole track. Fragmented
            // MP4 (HLS) may report zero; keep the provider duration then.
            if let Some(duration) = source.total_duration().filter(|d| position.is_zero() && !d.is_zero()) {
                track.duration_ms.store(duration.as_millis().min(u64::MAX as u128) as u64, Ordering::Relaxed);
            }
            let mut source = UniformSourceIterator::new(source, channels, rate);
            loop {
                if stopped() { return; }
                if let Some((new_epoch, new_position)) = seek_rx.try_iter().last() {
                    epoch = new_epoch; position = new_position; continue 'restart;
                }
                let mut samples = recycled.try_recv().unwrap_or_else(|_| Vec::with_capacity(BLOCK_FRAMES * channels as usize));
                samples.clear();
                for _ in 0..BLOCK_FRAMES * channels as usize {
                    match source.next() { Some(sample) => samples.push(sample), None => break }
                }
                let ended = samples.is_empty();
                let mut packet = if ended { Packet::End(epoch) } else { Packet::Samples(epoch, samples) };
                loop {
                    if stopped() { return; }
                    if let Some((new_epoch, new_position)) = seek_rx.try_iter().last() {
                        epoch = new_epoch; position = new_position; continue 'restart;
                    }
                    match sender.send_timeout(packet, Duration::from_millis(20)) {
                        Ok(()) => break,
                        Err(crossbeam_channel::SendTimeoutError::Timeout(p)) => packet = p,
                        Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => return,
                    }
                }
                if !ended && !announced_ready {
                    announced_ready = true;
                    let _ = worker_events.send(PlayerEvent::PreloadReady { track_id: track.metadata.id.clone() });
                }
                if ended {
                    // Keep the decoder worker seekable while its final PCM is
                    // still queued. Sleep rather than busy-polling at EOF.
                    loop {
                        if stopped() { return; }
                        match seek_rx.recv_timeout(Duration::from_millis(20)) {
                            Ok((e, p)) => { epoch = e; position = p; continue 'restart; }
                            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
                            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                        }
                    }
                }
            }
        }
    });
    if let Err(error) = spawn {
        let _ = events.send(PlayerEvent::Error { track_id: Some(track.metadata.id.clone()), message: error.to_string() });
    }
    let buffer = NextTrackBuffer { track: track.clone(), packets, recycle, seeks,
        samples: Vec::new(), offset: 0, epoch: 0, position: 0, listened: 0, started: false, last_progress: 0 };
    (track, buffer)
}

pub(crate) enum Control { Append(NextTrackBuffer), Clear, Skip, Seek(Duration) }

pub(crate) struct Pipeline {
    controls: Receiver<Control>,
    shared: Arc<Shared>,
    events: broadcast::Sender<PlayerEvent>,
    current: Option<NextTrackBuffer>,
    queue: VecDeque<NextTrackBuffer>,
    channels: u16,
    rate: u32,
    until_control: usize,
    fade_position: u64,
    fade_length: u64,
    paused: bool,
    volume: f32,
    ramp: Ramp,
    /// Sample index within the current output frame.
    channel: usize,
    silence_remaining: usize,
}

impl Pipeline {
    pub fn new(controls: Receiver<Control>, shared: Arc<Shared>, events: broadcast::Sender<PlayerEvent>, channels: u16, rate: u32) -> Self {
        // Start at the stored volume: no audible glide from 1.0 on startup.
        let volume = f32::from_bits(shared.volume.load(Ordering::Relaxed));
        Self { controls, shared, events, current: None, queue: VecDeque::with_capacity(2), channels, rate,
            until_control: 0, fade_position: 0, fade_length: 0, paused: false, volume, ramp: Ramp::new(volume), channel: 0, silence_remaining: 0 }
    }

    fn samples_per_second(&self) -> u64 { self.channels as u64 * self.rate as u64 }

    fn finish(&mut self, skipped: bool) {
        if let Some(mut current) = self.current.take() {
            current.end(&self.events, self.samples_per_second(), skipped);
        }
        self.fade_position = 0;
        self.fade_length = 0;
    }

    fn controls(&mut self) {
        // Limit work per render quantum even if commands arrive continuously.
        for _ in 0..64 {
            let Ok(command) = self.controls.try_recv() else { break; };
            match command {
                Control::Append(buffer) => self.queue.push_back(buffer),
                Control::Clear => {
                    self.finish(true);
                    let sps = self.samples_per_second();
                    for mut buffer in self.queue.drain(..) {
                        if buffer.started { buffer.end(&self.events, sps, true); }
                    }
                }
                Control::Skip => {
                    if self.current.is_none() { self.current = self.queue.pop_front(); }
                    self.finish(true);
                }
                Control::Seek(position) => {
                    let sps = self.samples_per_second();
                    // During overlap two tracks have already been heard. Do
                    // not silently rewind B: reject ambiguous seeks explicitly.
                    if self.fade_length != 0 {
                        let _ = self.events.send(PlayerEvent::Error { track_id: None, message: "seek during crossfade is not supported".into() });
                    } else {
                        if self.current.is_none() { self.current = self.queue.pop_front(); }
                        if let Some(current) = self.current.as_mut() {
                            if !current.seek(position, sps) {
                                let _ = self.events.send(PlayerEvent::Error { track_id: Some(current.track.metadata.id.clone()), message: "seek queue full or decoder stopped".into() });
                            }
                        }
                    }
                }
            }
        }
        self.paused = self.shared.paused.load(Ordering::Relaxed);
        let target = f32::from_bits(self.shared.volume.load(Ordering::Relaxed));
        let frames = (self.rate as u64 * VOLUME_RAMP_MS / 1000).min(u32::MAX as u64) as u32;
        self.ramp.retarget(target, frames);
    }
}

impl Iterator for Pipeline {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        if self.until_control == 0 {
            if self.shared.stopped.load(Ordering::Acquire) { return None; }
            self.controls();
            self.until_control = 256 * self.channels as usize;
        }
        self.until_control -= 1;
        // Output is frame-aligned (underruns pad whole frames), so the gain
        // steps once per frame and every channel of a frame gets the same one.
        if self.channel == 0 { self.volume = self.ramp.advance(); }
        self.channel = (self.channel + 1) % self.channels.max(1) as usize;
        if self.paused { return Some(0.0); }
        if self.silence_remaining > 0 {
            self.silence_remaining -= 1;
            return Some(0.0);
        }
        let sps = self.samples_per_second();
        let mut boundary = None;
        loop {
            if self.current.is_none() { self.current = self.queue.pop_front(); }
            let Some(current) = self.current.as_mut() else {
                self.silence_remaining = self.channels as usize - 1;
                return Some(0.0);
            };
            // The source is kept alive while idle (see below), so the output
            // stream/sink never needs to be recreated between tracks.
            match current.poll() {
                Poll::Pending => {
                    // Resume only on a frame boundary; otherwise an underrun
                    // could swap left/right channels when a packet arrives.
                    self.silence_remaining = self.channels as usize - 1;
                    return Some(0.0);
                }
                Poll::End => {
                    if current.started { boundary = Some(current.track.metadata.id.clone()); }
                    self.finish(false);
                    continue;
                }
                Poll::Sample(a) => {
                    if let Some(from_track_id) = boundary.take() {
                        let _ = self.events.send(PlayerEvent::GaplessTransition {
                            from_track_id, to_track_id: current.track.metadata.id.clone(),
                        });
                    }
                    current.consumed(&self.events, sps);
                    let duration = current.track.duration_ms.load(Ordering::Relaxed);
                    let total = duration.saturating_mul(sps) / 1000;
                    let remaining = total.saturating_sub(current.position);
                    let fade = self.shared.crossfade_ms.load(Ordering::Relaxed).saturating_mul(sps) / 1000;
                    if self.fade_length == 0 && fade > 0 && duration > 0 && remaining < fade
                        && current.position % self.channels as u64 == 1 % self.channels as u64 {
                        if let Some(next) = self.queue.front() {
                            // Avoid starting a fade into an unprepared decoder.
                            if !next.packets.is_empty() || next.offset < next.samples.len() {
                                self.fade_length = (remaining + 1).max(self.channels as u64);
                            }
                        }
                    }
                    if self.fade_length > 0 {
                        if let Some(next) = self.queue.front_mut() {
                            match next.poll() {
                                Poll::Sample(b) => {
                                    next.consumed(&self.events, sps);
                                    // Same gain for every channel of a frame.
                                    let frame = self.fade_position / self.channels as u64;
                                    let frames = (self.fade_length / self.channels as u64).max(1);
                                    let gain = (frame as f32 / frames.saturating_sub(1).max(1) as f32).min(1.0);
                                    self.fade_position += 1;
                                    return Some((a * (1.0 - gain) + b * gain) * self.volume);
                                }
                                Poll::End => {
                                    if let Some(mut next) = self.queue.pop_front() { next.end(&self.events, sps, false); }
                                    self.fade_length = 0; self.fade_position = 0;
                                }
                                Poll::Pending => {}
                            }
                        }
                    }
                    return Some(a * self.volume);
                }
            }
        }
    }
}

impl Source for Pipeline {
    fn current_span_len(&self) -> Option<usize> { None }
    fn channels(&self) -> u16 { self.channels }
    fn sample_rate(&self) -> u32 { self.rate }
    fn total_duration(&self) -> Option<Duration> { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(id: &str, data: Vec<f32>, duration_ms: u64) -> NextTrackBuffer {
        let track = Arc::new(Track {
            metadata: TrackMetadata { id: id.into(), title: id.into(), artists: vec![],
                album: None, duration_ms, cover_url: None, isrc: None },
            finished: AtomicBool::new(false), cancelled: Arc::new(AtomicBool::new(false)),
            duration_ms: AtomicU64::new(duration_ms),
        });
        let (tx, packets) = bounded(4);
        tx.send(Packet::Samples(0, data)).unwrap();
        tx.send(Packet::End(0)).unwrap();
        let (recycle, _) = bounded(4);
        let (seeks, _) = bounded(4);
        NextTrackBuffer { track, packets, recycle, seeks, samples: vec![], offset: 0,
            epoch: 0, position: 0, listened: 0, started: false, last_progress: 0 }
    }

    fn pipeline() -> (Sender<Control>, Pipeline, broadcast::Receiver<PlayerEvent>) {
        let (tx, rx) = bounded(8);
        let (events, receiver) = broadcast::channel(32);
        (tx, Pipeline::new(rx, Arc::new(Shared::default()), events, 1, 1000), receiver)
    }

    #[test]
    fn prepared_tracks_join_without_an_inserted_silent_sample() {
        let (tx, mut source, mut events) = pipeline();
        assert!(tx.send(Control::Append(buffer("a", vec![0.2, 0.3], 2))).is_ok());
        assert!(tx.send(Control::Append(buffer("b", vec![0.4, 0.5], 2))).is_ok());
        assert_eq!(source.by_ref().take(5).collect::<Vec<_>>(), vec![0.2, 0.3, 0.4, 0.5, 0.0]);
        assert!(matches!(events.try_recv().unwrap(), PlayerEvent::TrackStarted { track } if track.id == "a"));
        assert!(matches!(events.try_recv().unwrap(), PlayerEvent::TrackEnded { completion_ratio: 1.0, skipped_early: false, .. }));
        assert!(matches!(events.try_recv().unwrap(), PlayerEvent::GaplessTransition { from_track_id, to_track_id }
            if from_track_id == "a" && to_track_id == "b"));
        assert!(matches!(events.try_recv().unwrap(), PlayerEvent::TrackStarted { track } if track.id == "b"));
        assert!(matches!(events.try_recv().unwrap(), PlayerEvent::TrackEnded { completion_ratio: 1.0, skipped_early: false, .. }));
        assert_eq!(source.next(), Some(0.0)); // idle must not terminate Source
    }

    #[test]
    fn empty_track_does_not_announce_a_gapless_transition() {
        let (tx, mut source, mut events) = pipeline();
        assert!(tx.send(Control::Append(buffer("empty", vec![], 1))).is_ok());
        assert!(tx.send(Control::Append(buffer("audible", vec![0.4], 1))).is_ok());
        assert_eq!(source.next(), Some(0.4));
        while let Ok(event) = events.try_recv() {
            assert!(!matches!(event, PlayerEvent::GaplessTransition { .. }));
        }
    }

    #[test]
    fn pause_does_not_consume_pcm_and_skip_reports_listened_fraction() {
        let (tx, mut source, mut events) = pipeline();
        assert!(tx.send(Control::Append(buffer("a", vec![0.2; 4], 4))).is_ok());
        assert_eq!(source.next(), Some(0.2));
        source.shared.paused.store(true, Ordering::Relaxed);
        source.until_control = 0;
        assert_eq!(source.by_ref().take(20).collect::<Vec<_>>(), vec![0.0; 20]);
        assert_eq!(source.current.as_ref().unwrap().listened, 1);
        assert!(tx.send(Control::Skip).is_ok());
        source.until_control = 0;
        source.next();
        events.try_recv().unwrap();
        assert!(matches!(events.try_recv().unwrap(), PlayerEvent::TrackEnded {
            completion_ratio: 0.25, skipped_early: true, ..
        }));
    }

    #[test]
    fn crossfade_blends_and_does_not_replay_the_next_tracks_prefix() {
        let (tx, mut source, _) = pipeline();
        source.shared.crossfade_ms.store(2, Ordering::Relaxed);
        assert!(tx.send(Control::Append(buffer("a", vec![1.0; 4], 4))).is_ok());
        assert!(tx.send(Control::Append(buffer("b", vec![0.0; 4], 4))).is_ok());
        let pcm = source.by_ref().take(5).collect::<Vec<_>>();
        assert_eq!(pcm, vec![1.0, 1.0, 1.0, 0.0, 0.0]);
        assert_eq!(source.current.as_ref().unwrap().track.metadata.id, "b");
        assert_eq!(source.current.as_ref().unwrap().listened, 3);
    }

    #[test]
    fn volume_ramp_is_linear_and_retargets_from_the_applied_gain() {
        let mut ramp = Ramp::new(1.0);
        ramp.retarget(0.0, 4);
        let gains: Vec<f32> = (0..5).map(|_| ramp.advance()).collect();
        assert_eq!(gains, vec![0.75, 0.5, 0.25, 0.0, 0.0]);
        ramp.retarget(1.0, 4);
        assert_eq!(ramp.advance(), 0.25);
        ramp.retarget(0.25, 3); // slider moved back mid-ramp: glide, no jump
        assert!((ramp.advance() - 0.25).abs() < 1e-6);
        ramp.retarget(0.6, 0);
        assert_eq!(ramp.advance(), 0.6);
    }

    #[test]
    fn volume_change_ramps_with_one_gain_per_frame() {
        let (tx, rx) = bounded(8);
        let (events, _receiver) = broadcast::channel(32);
        // Stereo at 100 Hz: the 20 ms ramp spans 2 frames.
        let mut source = Pipeline::new(rx, Arc::new(Shared::default()), events, 2, 100);
        assert!(tx.send(Control::Append(buffer("a", vec![1.0; 8], 40))).is_ok());
        assert_eq!(source.next(), Some(1.0));
        source.shared.volume.store(0.0f32.to_bits(), Ordering::Relaxed);
        source.until_control = 0;
        assert_eq!(source.by_ref().take(5).collect::<Vec<_>>(), vec![1.0, 0.5, 0.5, 0.0, 0.0]);
    }

    #[test]
    fn seek_discards_old_generation_without_counting_skipped_audio() {
        let mut buffer = buffer("a", vec![0.1; 8], 1000);
        let (seeks, rx) = bounded(4);
        buffer.seeks = seeks;
        buffer.listened = 20;
        assert!(buffer.seek(Duration::from_millis(800), 1000));
        assert_eq!(rx.try_recv().unwrap(), (1, Duration::from_millis(800)));
        assert_eq!(buffer.position, 800);
        assert_eq!(buffer.listened, 20);
        assert!(matches!(buffer.poll(), Poll::End)); // stale samples never play
    }
}