//! Anonymous public metadata and a media availability probe only.
//! No playback, credentials, restriction bypass, or signed URL output.
use core_types::{MetadataProvider, StreamResolver};
use resolvers::soundcloud::SoundCloud;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let input = std::env::args().nth(1)
        .unwrap_or_else(|| "https://soundcloud.com/forss/flickermood".into());
    let result = async {
        let resolver = SoundCloud::new()?;
        let track = resolver.track(&input).await?;
        println!("Public metadata available; duration_ms={}", track.duration_ms);
        let stream = resolver.resolve(&track).await?;
        println!("Media availability probe passed; codec={:?}. Playback/HLS segments not tested.", stream.codec);
        Ok::<_, core_types::ResolverError>(())
    }.await;
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            // NotFound/InvalidInput can contain caller input; do not echo it.
            match error {
                core_types::ResolverError::NotFound { .. } => eprintln!("SoundCloud: no matching public track"),
                core_types::ResolverError::InvalidInput(_) => eprintln!("SoundCloud: invalid input"),
                _ => eprintln!("{error}"),
            }
            std::process::ExitCode::FAILURE
        }
    }
}
