//! Replaceable decoder backend: native Rodio/Symphonia by default, explicit
//! FFmpeg opt-in for WebM/Opus, HLS and servers without HTTP Range support.
//! `StreamSource::Local` streams are downloaded files at absolute paths.
use crate::{hls::HlsReader, mp4, streamer::{HttpReader, SplicedReader}, EngineError, Result};
use core_types::{AudioCodec, StreamInfo, StreamSource};
use rodio::{decoder::DecoderError, Decoder, Source};
use std::{fs::File, io::{BufReader, Read}, path::{Path, PathBuf}, process::{Child, ChildStdout, Command, Stdio},
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
        let context = describe(stream);
        if stream.source == StreamSource::Local { return open_local(stream, position, &context); }
        if stream.hls {
            // Forward-only stream: start at the segment containing `position`,
            // then discard the remaining offset as decoded frames.
            let (reader, skip) = HlsReader::open(self.client.clone(), &stream.stream_url, runtime.clone(), cancelled, position)
                .map_err(|e| EngineError::Message(format!("native HLS decode failed ({context}): {e}")))?;
            let hint = if stream.codec == AudioCodec::Mp3 { "mp3" } else { "mp4" };
            let decoder = Decoder::builder().with_data(BufReader::new(reader))
                .with_seekable(false).with_hint(hint).with_gapless(true).build()
                .map_err(|e| EngineError::Message(format!("native HLS decode failed ({context}): {}", decoder_error(&e))))?;
            return Ok(Box::new(decoder.skip_duration(skip)));
        }
        let reader = HttpReader::open(self.client.clone(), stream.stream_url.clone(), runtime.clone(), cancelled)?;
        let fail = |container: &str, e: &dyn std::fmt::Display|
            EngineError::Message(format!("native decode failed ({context}, {container}): {e}"));
        if let Some(layout) = mp4::layout(reader.head()).filter(|l| l.fragmented) {
            // Fragmented MP4 (YouTube DASH m4a): Symphonia's seekable mode would
            // scan every top-level atom, i.e. download the whole file before
            // the first sample. Feed init + the fragment holding `position`
            // forward-only instead, then skip the remainder as decoded PCM.
            let (resume, start) = match &layout.sidx {
                Some(sidx) => sidx.fragment_at(position),
                // No index: decode from the first fragment (slow but correct).
                None => (layout.init_end, Duration::ZERO),
            };
            if resume < layout.init_end || resume > reader.len() {
                return Err(fail("fragmented MP4", &"fragment index points outside the file"));
            }
            let reader = SplicedReader::new(reader, layout.init_end, resume);
            let decoder = Decoder::builder().with_data(BufReader::new(reader))
                .with_seekable(false).with_hint("mp4").with_gapless(true).build()
                .map_err(|e| fail("fragmented MP4", &decoder_error(&e)))?;
            return Ok(Box::new(decoder.skip_duration(position.saturating_sub(start))));
        }
        // Progressive MP4 (moov + one mdat) and MP3: the seekable scan only
        // touches a few headers, and random access gives exact seeks.
        let container = if reader.head().get(4..8) == Some(b"ftyp") { "MP4" }
            else if stream.codec == AudioCodec::Mp3 { "MP3" } else { "unknown container" };
        let length = reader.len();
        let mut decoder = Decoder::builder().with_data(BufReader::new(reader))
            .with_byte_len(length).with_seekable(true).with_gapless(true).build()
            .map_err(|e| fail(container, &decoder_error(&e)))?;
        if !position.is_zero() {
            decoder.try_seek(position).map_err(|e| fail(container, &format!("seek failed: {e}")))?;
        }
        Ok(Box::new(decoder))
    }
}

/// Downloaded files only; a relative path would resolve against whatever the
/// process working directory happens to be.
fn local_path(stream: &StreamInfo) -> Result<&Path> {
    let path = Path::new(&stream.stream_url);
    if stream.stream_url.is_empty() || !path.is_absolute() {
        return Err(EngineError::Message("local stream must be an absolute file path".into()));
    }
    Ok(path)
}

/// Whole-file random access is cheap on disk, so even fragmented MP4 (saved
/// from HLS/DASH) opens in seekable mode: Symphonia's isomp4 scans the moof
/// atoms forward during the seek, which gives exact positions.
fn open_local(stream: &StreamInfo, position: Duration, context: &str) -> Result<AudioSource> {
    let path = local_path(stream)?;
    let fail = |e: &dyn std::fmt::Display| EngineError::Message(format!("native decode failed ({context}): {e}"));
    let hint = match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("mp3") => "mp3",
        Some("m4a" | "mp4") => "mp4",
        _ => if stream.codec == AudioCodec::Mp3 { "mp3" } else { "mp4" },
    };
    let build = || -> Result<Decoder<BufReader<File>>> {
        // Do not echo the path: it can contain the user's account name.
        let file = File::open(path).map_err(|e| fail(&format!("cannot open file: {}", e.kind())))?;
        let length = file.metadata().map_err(|e| fail(&format!("cannot read file size: {}", e.kind())))?.len();
        Decoder::builder().with_data(BufReader::new(file)).with_byte_len(length).with_seekable(true)
            .with_hint(hint).with_gapless(true).build().map_err(|e| fail(&decoder_error(&e)))
    };
    let mut decoder = build()?;
    if position.is_zero() || decoder.try_seek(position).is_ok() { return Ok(Box::new(decoder)); }
    // A failed seek may leave the demuxer mid-file: reopen and discard PCM.
    Ok(Box::new(build()?.skip_duration(position)))
}

/// Codec and service for error messages; never the (signed) URL.
fn describe(stream: &StreamInfo) -> String {
    let codec = match stream.codec { AudioCodec::Aac => "AAC", AudioCodec::Mp3 => "MP3", AudioCodec::Opus => "Opus" };
    let source = match stream.source { StreamSource::YouTube => "YouTube", StreamSource::SoundCloud => "SoundCloud", StreamSource::Local => "local file" };
    format!("{codec}, {source}")
}

/// Rodio's `Display` collapses unsupported codecs to "Unrecognized format";
/// make that case explicit.
fn decoder_error(error: &DecoderError) -> String {
    match error {
        DecoderError::UnrecognizedFormat => "unrecognized or unsupported format".into(),
        other => other.to_string(),
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
        let local = stream.source == StreamSource::Local;
        let input = if local {
            // `file:` keeps a Windows drive letter from being parsed as a protocol.
            format!("file:{}", local_path(stream)?.display())
        } else if stream.stream_url.starts_with("https://") || stream.stream_url.starts_with("http://") {
            stream.stream_url.clone()
        } else {
            return Err(EngineError::Message("FFmpeg input must be an HTTP(S) URL".into()));
        };
        let position = position.as_secs_f64().to_string();
        // HTTP options stay off for files: unused input options fail the open.
        let input_args: Vec<&str> = if local { vec!["-protocol_whitelist", "file"] } else {
            vec!["-rw_timeout", "10000000", "-user_agent", core_types::MEDIA_USER_AGENT,
                "-protocol_whitelist", "http,https,tcp,tls,crypto"]
        };
        let mut command = Command::new(&self.executable);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        let mut child = command.args(["-nostdin", "-v", "error"]).args(&input_args)
            .args(["-ss", &position, "-i", &input,
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
#[cfg(test)]
mod tests {
    use super::*;

    fn local(path: &str, codec: AudioCodec) -> StreamInfo {
        StreamInfo { stream_url: path.into(), codec, bitrate: None, source: StreamSource::Local, expires_at: None, hls: false }
    }

    #[test]
    fn local_streams_require_absolute_paths() {
        for relative in ["", "song.m4a", "music/song.mp3", "./song.mp3"] {
            assert!(local_path(&local(relative, AudioCodec::Aac)).is_err(), "{relative}");
        }
        let absolute = std::env::temp_dir().join("youtify-song.m4a");
        assert!(local_path(&local(absolute.to_str().unwrap(), AudioCodec::Aac)).is_ok());
    }

    #[test]
    fn missing_local_file_error_does_not_echo_the_path() {
        let path = std::env::temp_dir().join("youtify-missing-7f3a9c").join("private-name.m4a");
        let stream = local(path.to_str().unwrap(), AudioCodec::Aac);
        let Err(error) = open_local(&stream, Duration::ZERO, &describe(&stream)) else { panic!("opened a missing file") };
        let message = error.to_string();
        assert!(message.contains("local file") && !message.contains("private-name"), "{message}");
    }

    #[test]
    fn ffmpeg_rejects_relative_local_and_non_http_inputs() {
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let ffmpeg = FfmpegDecoder::new("ffmpeg-not-started");
        for stream in [local("song.webm", AudioCodec::Opus),
            StreamInfo { source: StreamSource::YouTube, ..local("file:///etc/passwd", AudioCodec::Opus) }] {
            let Err(error) = ffmpeg.open(&stream, Duration::ZERO, runtime.handle(), Arc::default()) else { panic!("accepted input") };
            assert!(!error.to_string().contains("Could not start"), "{error}");
        }
    }
}
