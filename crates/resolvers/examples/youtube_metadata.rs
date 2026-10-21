//! Public metadata only; no media download, credentials, storage, or Botguard.
use rustypipe::{client::RustyPipe, model::YouTubeItem};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The transport mode exposes reqwest's source chain, which RustyPipe discards.
    let mode = std::env::args().nth(1).unwrap_or_else(|| "search".into());
    if mode == "transport" {
        let response = reqwest::Client::builder().timeout(std::time::Duration::from_secs(20))
            .build()?.get("https://music.youtube.com/").send().await?;
        eprintln!("visitor-data endpoint status: {}", response.status());
        return Ok(());
    }
    let pipe = RustyPipe::builder()
        .no_botguard().no_storage().no_reporter().unauthenticated().build()?;
    match mode.as_str() {
        "search" => {
            eprintln!("search metadata");
            let search = pipe.query().search::<YouTubeItem, _>("Big Buck Bunny Blender").await?;
            eprintln!("search returned {} items", search.items.items.len());
        }
        "player" => {
            eprintln!("player metadata");
            let player = pipe.query().player("aqz-KE-bpKQ").await?;
            eprintln!("player returned {} audio streams", player.audio_streams.len());
        }
        _ => return Err("expected search, player, or transport".into()),
    }
    Ok(())
}
