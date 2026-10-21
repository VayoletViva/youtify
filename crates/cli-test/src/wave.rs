use audio_engine::PlayerEvent;
use core_types::TrackMetadata;
use resolvers::spotify::Spotify;
use wave_engine::{
    discover_tracks, features::vectorize, unix_timestamp, Interaction, WaveConfig,
    WaveDb, WaveQueue,
};

pub async fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut queries = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg != "--discover" { return Err(format!("unknown wave option: {arg}").into()); }
        queries.push(args.next().ok_or("--discover requires one quoted search query")?.clone());
    }
    if queries.len() > 8 { return Err("at most 8 discovery queries per invocation".into()); }
    // All SQLite initialization/ranking is off the Tokio runtime workers.
    let mut db = tokio::task::spawn_blocking(seed_demo).await??;
    if !queries.is_empty() {
        let provider = Spotify::new()?;
        // This explicit, bounded operation is not part of playback/ranking.
        let report = discover_tracks(&db, &provider, &queries).await?;
        println!("Discovery: {} new tracks", report.inserted);
        for error in report.errors { eprintln!("Discovery: {error}"); }
    }
    tokio::task::spawn_blocking(move || -> wave_engine::Result<()> {
        let now = unix_timestamp();
        for (index, (id, completion_ratio, skipped_early, liked)) in [
            ("wave-demo:chill", 0.98, false, true),
            ("wave-demo:ambient", 0.92, false, false),
            ("wave-demo:phonk", 0.08, true, false),
            ("wave-demo:metal", 0.10, true, false),
        ].into_iter().enumerate() {
            let event = PlayerEvent::TrackEnded {
                track_id: id.into(), completion_ratio, skipped_early,
            };
            println!("Simulated {event:?}; liked={liked}");
            record_player_event(&mut db, event, liked, now.saturating_sub(240 - index as i64 * 60))?;
        }
        let config = WaveConfig::default();
        let mut queue = WaveQueue::new(6)?;
        queue.refill(&db, now, &config)?;
        println!("\nMy Wave (wave.db; exploration={:.0}%):", config.exploration_rate * 100.0);
        let mut index = 0;
        // Drain the generated batch, not an infinite autoplay loop. Reservations
        // are released by a real player only when a track ends or fails to load.
        while let Some(item) = queue.pop_next() {
            index += 1;
            println!("{index:2}. score={:+.4} cosine={:+.4}  {} — {} [{}]",
                item.score, item.similarity, item.track.artists.join(", "),
                item.track.title, item.track.id);
        }
        Ok(())
    }).await??;
    Ok(())
}

fn seed_demo() -> wave_engine::Result<WaveDb> {
    let db = WaveDb::open("wave.db")?;
    for (id, title, artist, seconds) in [
        ("chill", "Chill Instrumental", "Quiet Rooms", 210),
        ("ambient", "Ambient Night", "Quiet Rooms", 260),
        ("phonk", "Phonk Rush", "Street Pulse", 150),
        ("metal", "Metal Storm", "Iron Sky", 230),
        ("lofi", "Lofi Chill Instrumental", "Evening Tape", 190),
        ("relax", "Relaxing Ambient", "Cloud Garden", 280),
        ("acoustic", "Acoustic Sad", "Woodland", 205),
        ("jazz", "Jazz Instrumental", "Blue Trio", 240),
        ("dance", "Electronic Dance", "Neon Circuit", 195),
        ("rock", "Rock Vocal", "Open Road", 220),
        ("piano", "Classical Instrumental", "Soft Keys", 270),
        ("house", "Techno House", "Night Grid", 330),
    ] {
        let track = TrackMetadata {
            id: format!("wave-demo:{id}"), title: title.into(), artists: vec![artist.into()],
            album: None, duration_ms: seconds * 1000, cover_url: None, isrc: None,
        };
        if !db.contains_track(&track.id)? {
            db.upsert_track(&track, vectorize(&track, &[], 0))?;
        }
    }
    Ok(db)
}

/// Adapter stays in the integration crate, so wave-engine does not depend on
/// CPAL/audio devices. Progress notifications never trigger SQLite writes.
fn record_player_event(
    db: &mut WaveDb,
    event: PlayerEvent,
    liked: bool,
    timestamp: i64,
) -> wave_engine::Result<()> {
    if let PlayerEvent::TrackEnded { track_id, completion_ratio, skipped_early } = event {
        db.record_interaction(&Interaction {
            track_id, completion_ratio, skipped_early, liked, timestamp,
        })?;
    }
    Ok(())
}