//! Public metadata only, using the existing anonymous resolver without credentials.
use core_types::{MetadataProvider, ResolverError};
use resolvers::spotify::Spotify;
use std::{process::ExitCode, time::{Duration, Instant}};

#[tokio::main]
async fn main() -> ExitCode {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "query".into());
    let input = match mode.as_str() {
        "query" => "Rick Astley Never Gonna Give You Up",
        "link" => "https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT",
        _ => {
            eprintln!("Usage: spotify_metadata [query|link]");
            return ExitCode::FAILURE;
        }
    };
    let started = Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(45), async {
        Spotify::new()?.track(input).await
    }).await;
    println!("mode={mode}; elapsed_ms={}; total_timeout_secs=45", started.elapsed().as_millis());
    match result {
        Ok(Ok(track)) => {
            println!("Public metadata available; duration_ms={}; artist_count={}; album_present={}; isrc_present={}",
                track.duration_ms, track.artists.len(), track.album.is_some(), track.isrc.is_some());
            ExitCode::SUCCESS
        }
        Ok(Err(error)) => {
            // Do not echo caller input, tokens, response bodies, or metadata URLs.
            match error {
                ResolverError::NotFound { .. } => eprintln!("Spotify: no matching public track"),
                ResolverError::InvalidInput(_) => eprintln!("Spotify: invalid input"),
                _ => eprintln!("{error}"),
            }
            ExitCode::FAILURE
        }
        Err(_) => {
            eprintln!("Spotify: total metadata timeout after 45 seconds");
            ExitCode::FAILURE
        }
    }
}
