# wave-engine

On-demand, local recommendations without neural networks or DSP decoding.
`rusqlite` uses bundled SQLite; the wave crate has no audio-device dependency.

## Offline demonstration

From the workspace root, run `cargo run -p cli-test -- --wave`.
This creates `wave.db` in the current directory, inserts twelve synthetic
metadata records, records four simulated `PlayerEvent::TrackEnded` events,
and prints six recommendations with ranking score and cosine similarity.
Synthetic IDs are not playable service IDs. Repeated runs preserve history
and append another set of simulated events; this is intentionally not a
production-library import command.

Explicit discovery is available via
`cargo run -p cli-test -- --wave --discover "adjacent artist track query"`.
It uses the existing `MetadataProvider` interface and inserts only new IDs.
At most eight queries are accepted, with a ten-second timeout per query.
The caller supplies adjacent-artist queries: the existing resolver contracts
do not provide an artist-similarity endpoint. No network call is made by ranking.

## Integration

1. Store metadata with `WaveDb::upsert_track` and `features::vectorize`.
   Optionally share one `TagClient` (with an identifying contact User-Agent)
   and use `vectorize_or_fallback` before storing the vector. MusicBrainz
   enrichment is opt-in; failures fall back to local title/duration heuristics.
2. Convert each `TrackEnded` event to one `Interaction`. Supply the UI's like
   state separately, since the audio event does not contain it. Do not feed
   high-frequency progress events into the database.
3. Call `WaveQueue::next_track` or `refill` on demand. Resolve returned metadata
   with the app's `StreamResolver` and send `PlayerCommand::QueueNext` from the
   integration layer. The demo only prints metadata; it does not play audio.
4. Call `release(id)` after completion or stream-resolution failure. Popped IDs
   remain reserved to avoid duplication while tracks are preloaded/in flight.
   After a recorded end event, the next refill recomputes the taste profile.

SQLite operations are synchronous. Own the database on a non-audio worker;
use `spawn_blocking` when integrating with an async app. The CLI moves its
seeding and ranking work there. Optional discovery does short SQLite writes
between async requests; do not call it from an audio callback.

## Mathematics and resource bounds

- Each stored vector is 16 little-endian `f32` values (64 bytes). Slots 0–11
  hash tags; slots 12–15 hold energy, estimated tempo, duration and novelty,
  normalized to [0,1] before L2 normalization. These are heuristics, not
  measured BPM, spectral features or real provider popularity statistics.
- Profile: last 50 events, +2 for a like, -1.5 for an early skip or completion
  below 0.15, +1 for completion above 0.8, otherwise zero. An explicit like
  takes precedence. Default exponential half-life is seven days. Repeated
  listens count as separate events. A cold-start profile is the zero vector.
- Score: cosine + `exploration_rate * (0.5 * novelty + uniform(-0.5, 0.5))`
  minus a recent-play penalty (default 1.0 within two hours). Novelty is
  `1/sqrt(1+play_count)`. Exploration defaults to 0.15; it is a bounded score
  perturbation, **not** a guarantee that exactly 15% of a batch is new artists.
  Zero exploration is deterministic; a seeded RNG API is also available.
- Candidate rows stream from SQLite into a bounded top-k heap. Metadata JSON
  is deserialized only for winners. Ranking is O(N * (16 + log k)), not an
  approximate nearest-neighbor index; memory is O(k + 50), excluding SQLite's
  approximately 1 MiB page cache and returned metadata. Batches/queues cap at
  100 tracks. There are no periodic timers, network polls or idle scan tasks.
- Interactions and counters are updated atomically. Upserts preserve history;
  invalid/non-finite ratios and vectors are rejected. The interaction log is
  persistent and grows with use; retention is an application policy, not an
  implicit deletion performed by this crate.

Bundled SQLite still requires a working C compiler for the target (Android:
an appropriately configured NDK toolchain). It avoids a runtime dependency
on an external SQLite installation; it does not configure cross-compilation.