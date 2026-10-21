# GitHub Installer Builds

`.github/workflows/build.yml` builds the checked-out UI and Rust application as
two independent jobs. It uploads workflow artifacts only: it does not create a
GitHub Release, publish to a store, commit files, or require signing secrets.

## Run and Download

1. Put this project at the root of a GitHub repository, including `.github`,
   `Cargo.lock`, `ui/package-lock.json`, the UI sources, and all workspace crates.
   Review the files for credentials before uploading. The ignore rules are not a
   secret scanner and do not remove files already tracked by Git.
2. Enable GitHub Actions. The workflow must be on the default branch for its
   manual trigger to appear. Open **Actions > Build Installers > Run workflow**
   and choose the branch to build.
3. Alternatively, pushing a tag matching `v*` triggers both jobs. Ordinary branch
   pushes and pull requests do not. Remove the workflow's `push` block if only
   manual builds are wanted. Tags do not change the app version automatically.
4. Download the ZIP archives from the completed run's **Artifacts** section and
   extract them. Artifacts expire after 14 days; they are not release assets.

| Artifact | Contents | Intended use |
| --- | --- | --- |
| `windows-x64-nsis-unsigned` | NSIS `.exe` installer, release profile, Windows x64 | Desktop testing; no Authenticode signature |
| `android-arm64-apk-debug-signed` | Debug `.apk`, arm64-v8a only | Sideload testing on compatible Android 8.0 / API 26+ devices |

Missing installers fail the upload step rather than silently succeeding. Android
also runs `apksigner verify` before uploading. Successful packaging/signature
verification does not establish working audio, networking, or mobile lifecycle
behavior; test on actual devices.

## Build Details

Both jobs install Node 22, stable Rust, and run `npm ci` in `ui`. This installs
the Tauri v2 CLI from the npm lockfile, rather than an unrelated global CLI.
The existing `npm run tauri` script changes to `crates/tauri-app`.
Tauri's `beforeBuildCommand` runs the TypeScript/Vite build and embeds `ui/dist`;
neither artifact needs a running Vite development server. Cargo builds use
`--locked`. Keep both lockfiles in source control and update them deliberately.

Windows uses the hosted `windows-2022` MSVC environment and explicitly builds
`x86_64-pc-windows-msvc` with `--bundles nsis --no-sign`. The upload path is the
workspace-level `target/x86_64-pc-windows-msvc/release/bundle/nsis/*.exe`, not a
crate-local target directory. The existing Rust build script generates desktop
icons. WebView2 is required at runtime; the default Tauri installer can bootstrap
it if missing, which may require internet access. Windows may warn about an
unknown publisher or SmartScreen reputation for this unsigned installer.

Android uses Ubuntu 24.04, Temurin Java 17 (`JAVA_HOME` supplied by setup-java),
the Android SDK (`ANDROID_HOME` supplied by setup-android), Build Tools 36.0.0,
and NDK 28.2.13676358. Both `NDK_HOME` and `ANDROID_NDK_HOME` point to that exact
installed NDK. Rust target `aarch64-linux-android` corresponds to Tauri's
`--target aarch64` and Android ABI `arm64-v8a`. This is not an x86 emulator APK
or a universal APK. Tauri configures the Android compiler/linker environment;
the NDK also supplies the C toolchain for bundled SQLite and native dependencies.

The minimum Android version is API 26 because CPAL links against AAudio.
`bundle.android.minSdkVersion` controls the Tauri linker API and CI's bindgen
target; CI also verifies that the NDK supplies `libaaudio.so` for this API.

Android Gradle invokes `npm run tauri` from `crates/tauri-app`, not `ui`.
The private `crates/tauri-app/package.json` forwards that callback to the CLI
installed in `ui/node_modules`. Keep this manifest in Git; it requires no
separate `npm install` or lockfile. UI dependencies remain locked in
`ui/package-lock.json`.

At inspection, `gen` contained schemas only, not an Android Gradle host. The
crate already exposes `tauri::mobile_entry_point` and the required library crate
types. CI runs `tauri android init --ci --skip-targets-install` if `gradlew` is
absent, then pins both `compileSdk` and `targetSdk` in the generated app Gradle
file to API 36 and installs that platform. `ANDROID_API_LEVEL` in the workflow
controls this pin. The CLI template can specify API 37 before that package is
available from sdkmanager; using the template value directly breaks CI.
Gradle may fetch additional SDK/build dependencies. SDK licenses are accepted by
setup-android. Network access to npm, crates.io, Codeberg (the pinned rustypipe
Git dependency), Google Maven/SDK, and Gradle distribution servers is required.

The root `.gitignore` deliberately excludes `crates/tauri-app/gen/android/`.
Generated host files are disposable for this baseline; they are not modified or
uploaded by this workflow. If native Android customizations become necessary,
review and track the host sources, remove this whole-directory ignore rule, and
keep build/cache directories, local SDK paths, and signing files ignored. The
conditional initialization then preserves a checked-in host. Review SDK, Gradle,
JDK, and NDK compatibility whenever updating the CLI lockfile or host template.

## Signing Limits

The APK is **debug-signed and installable**, not an unsigned release APK. Android
Gradle tooling creates a disposable debug key on the runner; no private signing
material is uploaded or cached. Debug builds are debuggable and are not intended
for production distribution. A later run can use a different debug certificate:
updating an existing installation may fail with a signature mismatch. Uninstalling
the previous test app resolves that mismatch but removes its local data.

Removing `--debug` does not create a production-signed APK. Release distribution
requires a persistent, securely managed signing identity, release Gradle signing
configuration, appropriate version codes, and device/store validation. An unsigned
release APK is not normally installable. Play distribution generally uses a signed
AAB and Play App Signing. None of that is configured here. Likewise, Windows
production signing requires a separate certificate/signing-service setup.

## Playback Limits

FFmpeg is **not bundled or installed** by this workflow or the application.
Desktop playback can use an external trusted `ffmpeg.exe`: provide an absolute
path through `WAVE_FFMPEG`, place it beside the installed app executable, or make
it discoverable through an absolute directory on `PATH`. Restart the app after
changing its environment. An invalid explicit override fails startup.

Without FFmpeg, desktop falls back to native decoding. Android always uses native
decoding and rejects a `WAVE_FFMPEG` override. Native mode does not support
SoundCloud HLS or Opus-only streams; tracks without a supported progressive
MP3/AAC stream can fail. Packaging the same UI does not provide desktop/mobile
playback feature parity or bypass provider/network restrictions.

Android remains a foreground Tauri app. Background playback guarantees, audio
focus, lock-screen MediaSession, and headset controls require further native
lifecycle/service work. Manually verify the generated manifest's INTERNET
permission, startup, search, playback, pause/seek/skip, and suspend/resume on a
physical arm64 device. Check desktop playback both with and without FFmpeg.

## Verification and References

The workflow was prepared by inspecting the configuration, lockfiles, build
script, mobile entry point, and playback implementation, and checking the local
CLI's Android command help. No installer/APK build or device test was performed
as part of preparing these files. The first GitHub run remains the end-to-end
validation, particularly for the native audio and SQLite Android dependency graph.
Stable Rust and hosted runner images can change; lockfiles alone do not pin the
entire build environment.

- [Tauri prerequisites and Android environment](https://v2.tauri.app/start/prerequisites/)
- [Tauri CLI build/init options](https://v2.tauri.app/reference/cli/)
- [Android production signing](https://v2.tauri.app/distribute/sign/android/)
- [Windows installers](https://v2.tauri.app/distribute/windows-installer/)
- [Locked CLI Android template](https://github.com/tauri-apps/tauri/blob/tauri-cli-v2.12.3/crates/tauri-cli/templates/mobile/android/app/build.gradle.kts)
