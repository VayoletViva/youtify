// Shapes shared by main.ts and the demo backend; they mirror the Tauri commands and the 'player-event' payloads.

export interface TrackMetadata {
  id: string;
  title: string;
  artists: string[];
  album: string | null;
  duration_ms: number;
  cover_url: string | null;
  isrc: string | null;
}
export interface Snapshot {
  revision: number;
  track: TrackMetadata | null;
  position_ms: number;
  duration_ms: number;
  paused: boolean;
  /** Linear gain 0..1 (the engine clamps it); the UI slider maps it through volume.ts. */
  volume: number;
  wave: boolean;
  /** My Wave is fetching new tracks online (Deezer chart, related artists). Older backends omit it. */
  discovering: boolean;
  next_track: TrackMetadata | null;
  preload: 'idle' | 'resolving' | 'buffering' | 'ready';
  queue: TrackMetadata[];
  error: string | null;
}
export interface DownloadItem { track: TrackMetadata; path: string; bytes: number }
export interface DownloadList { dir: string; items: DownloadItem[] }
export interface DownloadEvent {
  type: 'download';
  track_id: string;
  status: 'started' | 'progress' | 'done' | 'failed' | 'removed';
  done_bytes: number;
  total_bytes: number | null;
  message: string | null;
}
export type PlayerEvent = { type: 'state'; state: Snapshot }
  | { type: 'gapless'; from_track_id: string; to_track_id: string }
  | { type: 'error'; message: string }
  | DownloadEvent;
