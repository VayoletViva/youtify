use std::{process::ExitCode, time::Duration};
use core_types::{MetadataProvider, ResolverError, StreamInfo, StreamResolver};
use resolvers::{soundcloud::SoundCloud, spotify::Spotify, youtube::YouTube};
use audio_engine::{AudioEngine, PlayerCommand, PlayerEvent, decoder::FfmpegDecoder};
use std::sync::Arc;

mod wave;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => { eprintln!("Error: {error}"); ExitCode::FAILURE }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        println!("Usage: cli-test <Spotify track URL | search query>\n       cli-test --playlist <Spotify playlist URL>\n       cli-test --play [--ffmpeg] [--crossfade-ms N] <query-or-url> [<next-query-or-url>]\n       cli-test --wave [--discover <adjacent-artist-query> ...]\nQuote each query in --play mode. --ffmpeg requires FFmpeg on PATH (Opus/WebM/HLS). --wave is an offline demo unless --discover is supplied.");
        if args.is_empty() { return Err("a Spotify URL or search query is required".into()); }
        return Ok(());
    }
    if args[0] == "--wave" { return wave::run(&args[1..]).await; }
    let spotify = Spotify::new()?;
    if args[0] == "--play" { return play(&args[1..], &spotify).await; }
    if args[0] == "--playlist" {
        if args.len() != 2 { return Err("--playlist requires one Spotify playlist URL".into()); }
        let tracks = spotify.playlist(&args[1]).await?;
        println!("{}", serde_json::to_string_pretty(&tracks)?);
        return Ok(());
    }
    let track = spotify.track(&args.join(" ")).await?;
    println!("Metadata:\n{}", serde_json::to_string_pretty(&track)?);
    if track.isrc.is_none() { eprintln!("Spotify did not expose an ISRC; using artist/title search."); }
    let youtube = YouTube::new()?;
    let soundcloud = SoundCloud::new()?;
    let (youtube, soundcloud) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(180), youtube.resolve(&track)),
        tokio::time::timeout(Duration::from_secs(180), soundcloud.resolve(&track)),
    );
    let youtube = youtube.unwrap_or_else(|_| Err(ResolverError::Unavailable {
        service: "YouTube", message: "resolution timed out after 180s".into(),
    }));
    let soundcloud = soundcloud.unwrap_or_else(|_| Err(ResolverError::Unavailable {
        service: "SoundCloud", message: "resolution timed out after 180s".into(),
    }));
    print_result("YouTube", &youtube)?;
    print_result("SoundCloud", &soundcloud)?;
    if youtube.is_err() && soundcloud.is_err() { return Err("neither service returned a playable stream".into()); }
    Ok(())
}

async fn play(args: &[String], spotify: &Spotify) -> Result<(), Box<dyn std::error::Error>> {
    let mut ffmpeg = false;
    let mut crossfade = Duration::ZERO;
    let mut inputs = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--ffmpeg" => ffmpeg = true,
            "--crossfade-ms" => {
                let value: u64 = args.next().ok_or("--crossfade-ms requires milliseconds")?.parse()?;
                if value > 10_000 { return Err("crossfade must be 0..10000 ms".into()); }
                crossfade = Duration::from_millis(value);
            }
            value if value.starts_with("--") => return Err(format!("unknown playback flag: {value}").into()),
            _ => inputs.push(arg),
        }
    }
    if inputs.is_empty() || inputs.len() > 2 { return Err("--play requires one or two quoted queries/URLs".into()); }
    let youtube = YouTube::new()?;
    let soundcloud = SoundCloud::new()?;
    let mut tracks = Vec::new();
    // Resolve both before playback so resolver latency cannot spoil the first
    // transition. Only the engine downloads/decodes the media, in the background.
    for input in inputs {
        let track = spotify.track(input).await?;
        println!("Metadata:\n{}", serde_json::to_string_pretty(&track)?);
        let (yt, sc) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(180), youtube.resolve(&track)),
            tokio::time::timeout(Duration::from_secs(180), soundcloud.resolve(&track)),
        );
        let candidates = [yt, sc].into_iter().filter_map(|result| match result {
            Ok(Ok(stream)) => Some(stream),
            Ok(Err(error)) => { eprintln!("Resolver: {error}"); None }
            Err(error) => { eprintln!("Resolver timeout: {error}"); None }
        });
        let stream = candidates.into_iter().find(|stream| ffmpeg ||
            stream.codec != core_types::AudioCodec::Opus)
            .ok_or("no native-compatible stream; retry with --ffmpeg for Opus")?;
        tracks.push((track, stream));
    }
    let engine = if ffmpeg { AudioEngine::with_decoder(Arc::new(FfmpegDecoder::new("ffmpeg")))? }
        else { AudioEngine::new()? };
    let mut events = engine.subscribe();
    engine.send(PlayerCommand::SetCrossfade(crossfade))?;
    for (index, (track, stream)) in tracks.into_iter().enumerate() {
        engine.send(if index == 0 { PlayerCommand::Play(track, stream) } else { PlayerCommand::QueueNext(track, stream) })?;
    }
    let mut failed = false;
    loop {
        match events.recv().await {
            Ok(event) => {
                println!("{event:?}");
                if matches!(event, PlayerEvent::Error { .. }) { failed = true; }
                if matches!(event, PlayerEvent::QueueFinished) { break; }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                return Err(format!("lost {count} player events: event consumer too slow").into());
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Err("player event channel closed".into()),
        }
    }
    // CPAL reports samples when submitted, not when the DAC renders them.
    // Keep the device alive briefly to drain its final hardware buffer.
    tokio::time::sleep(Duration::from_millis(250)).await;
    if failed { return Err("playback reported errors; see PlayerEvent::Error".into()); }
    Ok(())
}

fn print_result(name: &str, result: &Result<StreamInfo, ResolverError>) -> Result<(), serde_json::Error> {
    match result {
        Ok(stream) => {
            // Signed media URLs can carry temporary authorization tokens.
            let mut diagnostic = serde_json::to_value(stream)?;
            diagnostic["stream_url"] = serde_json::Value::String("[redacted]".into());
            println!("{name}:\n{}", serde_json::to_string_pretty(&diagnostic)?);
        }
        Err(error) => eprintln!("{name}: {error}"),
    }
    Ok(())
}
