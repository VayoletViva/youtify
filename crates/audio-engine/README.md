# Audio engine

`AudioEngine::new()` must be created inside a running Tokio runtime. Subscribe
before submitting `Play(metadata, stream)`, then `QueueNext(metadata, stream)`.
Keep the engine and runtime alive for playback. The output device and sink are
opened once, even across pauses, skips, seeks and idle periods.

## Pipeline and bounds

- Native network reader: HTTP byte ranges, two cached 256 KiB windows and one
  asynchronous prefetched window per decoder. Requires HTTP 206, a valid
  Content-Range and known byte length; never downloads an entire file to RAM.
- Decode/resample on background threads, not the device callback. Current and
  next tracks each have 96 blocks of 1024 PCM frames, with recycled allocations
  (about 768 KiB / 2.05 seconds per stereo track at 48 kHz).
- Only current and next are preloaded; at most 128 further metadata entries.
  Commands and events are bounded too. No temporary audio files.
- The permanent source consumes the next ready buffer in the same `next()`
  call as EOF, without a timer or a new Sink. MP3/Vorbis gapless trimming is
  requested from Symphonia. Encoder padding support is format dependent;
  sample-perfect AAC trimming is not guaranteed by this backend.
- A stalled network can exhaust any bounded buffer. In that case output is
  silence, without blocking CPAL or advancing the listening cursor. Gapless
  playback requires the next decoder's PCM to be available in time.
- Crossfade is linear, optional, clamped to 10 seconds, and uses decoded
  duration when available (otherwise metadata). Unknown-duration streams do
  not crossfade. Seeking during an active overlap reports an explicit error.
  A seek rebuilds the decoder in its worker and can temporarily output silence.

## Codecs and external backend

Native Rodio/Symphonia enables `symphonia-all`: MP3, AAC, Vorbis/OGG, FLAC,
PCM/WAV and the associated containers. **Symphonia does not decode Opus.**
`DecoderBackend` allows supplying another implementation without changing
the pipeline or `StreamInfo`.

The included, explicitly selected `FfmpegDecoder` supports WebM/Opus (including
YouTube format 251), HLS and non-range servers when FFmpeg is installed. It
uses an external process per prepared track and a bounded PCM pipe. This mode
is heavier than native playback and is not enabled implicitly. FFmpeg startup
failures are reported; mid-stream decoder failures can appear as early EOF
because Rodio's Source interface has no fallible sample operation. There is
no automatic signed-URL refresh or provider fallback after decoding starts.

## Telemetry

TrackStarted is emitted at the first sample consumed by the output pipeline.
Progress is emitted every 250 ms of media position, not wall time. TrackEnded
is emitted once per played/explicitly skipped track. `completion_ratio` is
listened PCM duration divided by track duration, clamped to [0,1]; seeking
forward does not count as listening, repeated portions after seeking backward
do. Paused time and underrun silence do not count. For unknown duration,
natural EOF after audio gives 1, and skip gives 0. `skipped_early` records an
explicit Skip/replacement, not a transport failure. Events describe submitted
samples, not hardware DAC timestamps. Crossfade can start B before A ends.

Events use a bounded broadcast channel (256). Consumers must handle `Lagged`;
this is live telemetry, not a durable history store. The audio callback does
not perform network I/O or decoding, but event publication and buffer recycling
are not a strict hard-real-time/lock-free guarantee.

## CLI examples (from the workspace root)

```text
cargo run -p cli-test -- --play "artist - track one" "artist - track two"
cargo run -p cli-test -- --play --ffmpeg "artist - track one" "artist - track two"
cargo run -p cli-test -- --play --ffmpeg --crossfade-ms 1500 "track one" "track two"
```

The CLI resolves both tracks first, then starts playback/preloading and prints
PlayerEvent. Default native mode filters known Opus and `.m3u8` candidates;
use `--ffmpeg` when the provider only offers these formats. Queries with spaces
must be quoted. Existing metadata and playlist CLI modes are unchanged.

Linux builds require the ALSA development packages used by CPAL. Windows and
macOS use their native audio backends. Decoder workers observe cancellation;
an in-flight native request may take up to its 30-second timeout to exit, and
an FFmpeg read may wait for its 10-second network timeout. Drop stops output
immediately; it does not block waiting for network reads on decoder threads.