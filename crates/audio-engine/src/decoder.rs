//! Replaceable decoder backend: native Rodio/Symphonia by default, explicit
//! FFmpeg opt-in for WebM/Opus, HLS and servers without HTTP Range support.
use crate::{hls::HlsReader, streamer::HttpReader, EngineError, Result};
use core_types::{AudioCodec, StreamInfo};
use rodio::{Decoder, Source};
use std::{io::{BufReader, Read}, path::PathBuf, process::{Child, ChildStdout, Command, Stdio},
    sync::{Arc, atomic::AtomicBool}, time::Duration};
use tokio::runtime::Handle;

pub type AudioSource = Box<dyn Source + Send>;

pub trait DecoderBackend: Send + Sync + 'static {
    /// Called only on a decoder worker. The returned source must start at
    /// `position`; report unsupported seeks instead of silently ignoring them.
    fn open(&self, stream: &StreamInfo, position: Duration, runtime: &Handle,
        cancelled: Arc<AtomicBool>) -> Result<AudioSource>;
}

pub struct NativeDecoder { client: reqwest::Client }

impl NativeDecoder {
    pub fn new() -> Result<Self> { Ok(Self { client: HttpReader::client()? }) }
}

impl DecoderBackend for NativeDecoder {
    fn open(&self, stream: &StreamInfo, position: Duration, runtime: &Handle,
        cancelled: Arc<AtomicBool>) -> Result<AudioSource> {
        if stream.codec == AudioCodec::Opus {
            return Err(EngineError::Message("Opus needs an external DecoderBackend; CLI: --ffmpeg".into()));
        }
        if stream.hls {
            // Forward-only stream: start at the segment containing `position`,
            // then discard the remaining offset as decoded frames.
            let (reader, skip) = HlsReader::open(self.client.clone(), &stream.stream_url, runtime.clone(), cancelled, position)?;
            let hint = if stream.codec == AudioCodec::Mp3 { "mp3" } else { "mp4" };
            let decoder = Decoder::builder().with_data(BufReader::new(reader))
                .with_seekable(false).with_hint(hint).with_gapless(true).build()
                .map_err(|e| EngineError::Message(format!("native HLS decode failed: {e}")))?;
            return Ok(Box::new(decoder.skip_duration(skip)));
        }
        let reader = HttpReader::open(self.client.clone(), stream.stream_url.clone(), runtime.clone(), cancelled)?;
        let length = reader.len();
        let mut decoder = Decoder::builder().with_data(BufReader::new(reader))
            .with_byte_len(length).with_seekable(true).with_gapless(true).build()
            .map_err(|e| EngineError::Message(format!("native decode failed: {e}")))?;
        if !position.is_zero() {
            decoder.try_seek(position).map_err(|e| EngineError::Message(e.to_string()))?;
        }
        Ok(Box::new(decoder))
    }
}

/// Optional external backend, not used or started by default. Requires an
/// installed FFmpeg executable. PCM travels through a bounded OS pipe; no
/// temporary files or full-track buffers are created. FFmpeg handles Opus
/// pre-skip and container discard padding as well as segmented HLS.
pub struct FfmpegDecoder { executable: PathBuf }

impl FfmpegDecoder {
    pub fn new(executable: impl Into<PathBuf>) -> Self { Self { executable: executable.into() } }
}

impl DecoderBackend for FfmpegDecoder {
    fn open(&self, stream: &StreamInfo, position: Duration, _: &Handle,
        _: Arc<AtomicBool>) -> Result<AudioSource> {
        if !stream.stream_url.starts_with("https://") && !stream.stream_url.starts_with("http://") {
            return Err(EngineError::Message("FFmpeg input must be an HTTP(S) URL".into()));
        }
        let mut command = Command::new(&self.executable);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        let mut child = command.args(["-nostdin", "-v", "error", "-rw_timeout", "10000000",
                "-user_agent", core_types::MEDIA_USER_AGENT,
                "-protocol_whitelist", "http,https,tcp,tls,crypto",
                "-ss", &position.as_secs_f64().to_string(), "-i", &stream.stream_url,
                "-map", "0:a:0", "-vn", "-sn", "-dn", "-threads", "1",
                "-ac", "2", "-ar", "48000", "-f", "f32le", "pipe:1"])
            .stdin(Stdio::null()).stderr(Stdio::null()).stdout(Stdio::piped()).spawn()
            .map_err(|e| EngineError::Message(format!("Could not start FFmpeg; check the executable path and permissions: {e}")))?;
        let stdout = child.stdout.take().ok_or_else(|| EngineError::Message("FFmpeg stdout unavailable".into()))?;
        let mut source = FfmpegSource { child, output: BufReader::new(stdout), first: None };
        source.first = source.read_sample();
        if source.first.is_none() { return Err(EngineError::Message("FFmpeg produced no audio; check URL, executable and codec support".into())); }
        Ok(Box::new(source))
    }
}

struct FfmpegSource { child: Child, output: BufReader<ChildStdout>, first: Option<f32> }
impl FfmpegSource {
    fn read_sample(&mut self) -> Option<f32> {
        let mut bytes = [0; 4];
        self.output.read_exact(&mut bytes).ok()?;
        Some(f32::from_le_bytes(bytes))
    }
}
impl Iterator for FfmpegSource {
    type Item = f32;
    fn next(&mut self) -> Option<f32> { self.first.take().or_else(|| self.read_sample()) }
}
impl Source for FfmpegSource {
    fn current_span_len(&self) -> Option<usize> { None }
    fn channels(&self) -> u16 { 2 }
    fn sample_rate(&self) -> u32 { 48000 }
    fn total_duration(&self) -> Option<Duration> { None }
}
impl Drop for FfmpegSource {
    fn drop(&mut self) { let _ = self.child.kill(); let _ = self.child.wait(); }
}