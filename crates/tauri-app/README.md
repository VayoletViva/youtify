# Youtify · Tauri v2

Workspace: `C:\Users\User\.cline\data\workspaces\chat\player-core`.
Frontend: `C:\Users\User\.cline\data\workspaces\chat\player-core\ui`.
Tauri crate: `C:\Users\User\.cline\data\workspaces\chat\player-core\crates\tauri-app`.

## Manual setup and verification

No terminal commands, package installation, builds or tests were executed while
creating this integration. The commands below are for the maintainer to run later.
Use Node.js 22 LTS, a current stable Rust toolchain, and Tauri v2 platform
prerequisites (WebView2 + MSVC on Windows, WebKitGTK/GTK/ALSA development libraries
on Linux, Xcode tools on macOS).

From the frontend directory:

```text
npm install
npm test
npm run build
npm run tauri -- dev
npm run tauri -- build
```

From the workspace root, after the UI build:

```text
cargo test -p audio-engine
cargo test -p resolvers
cargo test -p tauri-app --lib
```

Commit the generated npm/Cargo lockfiles after the first verified installation.
The build script generates deterministic PNG/ICO/ICNS app icons before Tauri
code generation, using only Rust std. No missing binary icon files or external
image conversion commands are required for the desktop build.
The Tauri CLI enables `custom-protocol` for release builds. For a direct Cargo
release build, enable that feature explicitly and build the frontend first.

## Android

The library exposes `tauri::mobile_entry_point`, produces `staticlib`, `cdylib`
and `rlib`, and configures Android API 24+. Install Android Studio, SDK/NDK,
JDK and Rust Android targets following Tauri v2 prerequisites. From the frontend:

```text
npm run tauri -- android init
npm run tauri -- android dev
npm run tauri -- android build
```

`android init` must generate the Gradle/Kotlin host under the Tauri crate's
`gen/android` directory. It has deliberately NOT been executed or replaced with
hand-written generated files. Check the generated manifest's INTERNET permission.
For physical-device development, allow the Vite ports 1420/1421 through the local
firewall. The Vite configuration consumes `TAURI_DEV_HOST`; release CSP does not
include the development HTTP/WebSocket allowance.

This is a foreground Tauri app, not an Android foreground media service. Audio
focus, lock-screen MediaSession, headset controls and guaranteed playback while
the OS suspends the app need native Android lifecycle/service integration.
Desktop/Android audio-device initialization and actual playback require manual
testing; cross-platform configuration alone is not a verified Android binary.

## Optional desktop FFmpeg playback (SoundCloud HLS / Opus)

Desktop selects the decoder once at startup, in this order:

1. An explicit `WAVE_FFMPEG` absolute executable path. Invalid values fail startup
   rather than falling back. Do not include arguments or literal quote characters.
2. `ffmpeg.exe` (Windows) or `ffmpeg` (macOS/Linux) next to the app executable.
3. The same filename in `PATH` order, ignoring empty and relative entries.
4. The native decoder if no valid candidate is found.

Candidates must be absolute paths to existing files, with execute permission on
Unix or an `.exe` extension on Windows. Discovery does not launch anything or use
a shell, and does not verify binary contents. FFmpeg is not bundled, installed or
downloaded by the app. Use only trusted binaries and search directories.
Android/iOS always use native playback unless `WAVE_FFMPEG` is explicitly set,
in which case startup fails with an unsupported-option error; no discovery occurs.

Example in PowerShell, after placing FFmpeg at the path shown (adjust it to your
actual installation):

```powershell
$env:WAVE_FFMPEG = 'C:\Tools\ffmpeg\bin\ffmpeg.exe'
Set-Location 'C:\Users\User\.cline\data\workspaces\chat\player-core\ui'
npm run tauri -- dev
```

For a packaged app, launch its executable from that same configured PowerShell
session, or configure a persistent user environment variable and restart the
launcher. Setting this variable in an unrelated terminal does not affect an
already running application. Unset it with `Remove-Item Env:WAVE_FFMPEG` and
restart to use auto discovery. Native playback requires that no FFmpeg candidate
is present next to the app or in its absolute `PATH` directories.

With FFmpeg selected explicitly or discovered, both resolvers accept their existing HLS/Opus formats
and the engine uses FFmpeg for all streams. Full-length/duration and access checks
remain in force. An invalid path fails startup explicitly; an existing file that
is not a working FFmpeg binary fails at playback. No executable is launched by
the startup path check. FFmpeg is launched directly, without a shell, only on the
decoder worker. On Windows its console window is hidden. Signed URLs are passed
to the local child process, but FFmpeg stderr is not forwarded to the UI.

Manual checks (not performed by the assistant): play the failing SoundCloud track,
pause/resume, seek, skip while buffering, then play another track. Test an invalid
path, app-adjacent discovery, and PATH discovery. Restart without `WAVE_FFMPEG`
and without discoverable FFmpeg to verify native mode. Provider restrictions
and the separately reported YouTube resolver panic are not fixed by FFmpeg.

## Ownership and IPC

`AppState` is a Send + Sync facade over shared resolver instances, a bounded
32-request mailbox and an `Arc<RwLock<Snapshot>>`. Its dedicated owner thread
keeps `AudioEngine` (including its potentially !Send device stream) and SQLite
on one thread. There is no unsafe Send implementation and no DB operation in
the realtime audio callback. Network stream resolution uses separate Tokio tasks,
timeouts and a generation check; replacing a request aborts obsolete resolution.

Commands:

* `search_tracks(query, source)`: `spotify`, `soundcloud`, `spotify-playlist`,
  `soundcloud-playlist`. The existing metadata contract returns one best match
  for search, or a full playlist; YouTube has no public metadata-search contract.
* `play_track(track)`: prefers SoundCloud for SoundCloud tracks, falling back to
  YouTube; other tracks try YouTube first, then SoundCloud. Native mode selects
  progressive MP3/AAC; the optional FFmpeg mode also permits HLS/Opus;
  reports submission to the audio engine, not completion of asynchronous decoding.
* `control_playback(action, value)`: pause/resume/skip/seek/volume,
  case-insensitive. Seek is in **milliseconds**, volume is **0..1**.
* `start_my_wave()`: returns an initial local recommendation batch. An empty DB
  produces an actionable error; search/import first. Starting Wave during playback
  preserves the current/preloaded audio, replacing the remaining metadata queue.
* `get_player_state()`: revisioned snapshot for frontend reconnection.

`player-event` is a tagged union: `state`, `gapless`, `error`.
Preload starts when progress enters the final 10 seconds (immediately for short
tracks). A slow network can still miss the gapless deadline. `PreloadReady` means
the decoder has supplied its first PCM block, not that the entire track is loaded
or that network underruns are impossible. `GaplessTransition` is emitted by the
audio pipeline only when the next track supplies a sample at the boundary without
inserted silence. The UI does not infer gapless transitions from wall-clock time.

Listening/skipping statistics use the engine's sample-based completion ratio.
Tracks which never supplied an audible sample are not recorded as taste dislikes.
The database lives in Tauri's application-data directory, not the working directory.

## Deliberate limits

### Provider troubleshooting

* Spotify 429 honors the server's `Retry-After` in seconds, even for multi-hour
  restrictions. This is not a locally chosen 60-second limit. API and embed
  cooldowns are separate; a direct track link can still work if only the API is
  limited. Successful track embeds no longer trigger optional API enrichment.
  Anonymous title search is not guaranteed to work; these changes do not remove
  Spotify's restrictions. Cooldowns are in memory and reset on app restart,
  which does not reset the server's restriction.
* A replaced Play request receives an explicit cancellation, not a closed reply
  channel misreported as `Player worker stopped`. The UI ignores obsolete Play
  replies. Resolver tasks are joined so a task failure is reported and does not
  leave resolution permanently busy. This does not repair provider/network errors.
  SoundCloud HLS requires the optional desktop FFmpeg mode described above.
* Manual regression checks: click A then B while A resolves; only B's reply should
  update the status. Repeat a single SoundCloud track and capture the exact error
  if resolution fails. During an API 429, try one direct Spotify track link;
  do not repeatedly retry the limited search endpoint.

* The application-level `WaveEngine` wraps the actual crate exports (`WaveDb`,
  `UserTasteProfile`, `next_wave_batch`). Wave re-ranks the local catalog and
  replenishes bounded metadata batches; it does not invent online discovery APIs.
  Metadata vectors are heuristics, not measured audio features. After exhausting a
  small catalog it can repeat tracks. Unresolvable tracks are excluded until Wave
  is restarted. Search/import populates the persistent catalog.
* Search results are limited to 2,000 imported tracks; normal playlist playback
  retains the first 100 as its in-memory session list. The UI renders results in
  pages of 40. Wave ranks the whole stored catalog without loading it all into UI.
* Existing resolver defaults remain unchanged. New `new_native()` constructors
  select AAC/progressive MP3, avoiding the built-in decoder's unsupported Opus/HLS.
  No FFmpeg process is required in native mode. If providers offer only Opus or HLS
  streams, native playback fails visibly instead of selecting an unsupported
  format; use desktop FFmpeg playback for those formats. Restricted
  streams remain unavailable in either mode.
  Public anonymous provider endpoints can change or reject requests.
* `ui/src/theme.ts` uses 5-bit RGB histogram quantization of a 64×64 canvas and
  **OKLCH**, not Google's HCT algorithm. It gamut-maps tonal colors into sRGB and
  chooses a contrasting primary foreground. No color or UI framework is added.
  CORS-denied, missing, oversized or failed artwork falls back to a neutral seed.
* The UI uses a desktop rail/floating bar and a mobile bottom navigation/modal
  player sheet, safe-area insets, accessible native controls and reduced motion.
  No remote fonts, HTML injection, telemetry or web audio playback are used.
* The supplied test assertions cover color round-trips/contrast, unsafe artwork
  URLs, preload timing/control bounds and sample-contiguous gapless events.
  They have been inspected, **not executed**. Manual smoke checks: playback,
  seek/pause/skip, late resolution, provider errors, Wave cold start/refill,
  360px/768px/desktop layouts, CORS fallback and Android lifecycle behavior.
