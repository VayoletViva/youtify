// In-memory backend for the browser preview (?demo). No DOM, no network: it answers the same commands as the
// Tauri app and emits the same 'player-event' payloads, so every view state can be checked without Rust.
// Never used inside Tauri (main.ts guards it with !isTauri()).
import { clampSeek } from './format';
import type { DownloadItem, DownloadList, PlayerEvent, Snapshot, TrackMetadata } from './types';

export interface Timers {
  set(callback: () => void, ms: number): unknown;
  clear(handle: unknown): void;
}
export interface DemoOptions {
  /** Commands that reject with a network error the given number of times, to exercise error states
   *  (e.g. ?demo=downloads-error: { list_downloads: 2 } covers the start-up refresh and opening the view). */
  failTimes?: Record<string, number>;
}
export interface DemoBackend {
  command<T>(name: string, args?: Record<string, unknown>): Promise<T>;
  dispose(): void;
}

const track = (id: string, title: string, artist: string, album: string, seconds: number): TrackMetadata =>
  ({ id, title, artists: [artist], album, duration_ms: seconds * 1000, cover_url: null, isrc: null });

/** Public-domain pieces; durations are typical recordings. */
export const demoCatalog: TrackMetadata[] = [
  track('demo-satie-gymnopedie-1', 'Гимнопедия № 1', 'Эрик Сати', 'Trois Gymnopédies', 205),
  track('demo-debussy-clair-de-lune', 'Clair de Lune', 'Клод Дебюсси', 'Suite bergamasque', 302),
  track('demo-beethoven-moonlight-1', 'Лунная соната, ч. 1', 'Л. ван Бетховен', 'Соната № 14', 361),
  track('demo-pachelbel-canon', 'Канон ре мажор', 'И. Пахельбель', 'Canon and Gigue', 296),
  track('demo-grieg-morning', 'Утро', 'Э. Григ', 'Пер Гюнт, сюита № 1', 232),
  track('demo-schubert-ave-maria', 'Аве Мария', 'Ф. Шуберт', 'Ellens dritter Gesang', 284),
  // Plays into a decoder error, to check the error path of play_track.
  track('broken-demo-file', 'Битый файл (демо)', 'Повреждённая запись', 'Демо', 180),
];

const TICK_MS = 250;
const bytesFor = (t: TrackMetadata): number => Math.round((t.duration_ms / 1000) * 40_000); // ~320 kbit/s

export function createDemoBackend(
  emit: (event: PlayerEvent) => void,
  timers: Timers = { set: (callback, ms) => setTimeout(callback, ms), clear: handle => clearTimeout(handle as ReturnType<typeof setTimeout>) },
  options: DemoOptions = {},
): DemoBackend {
  const pending = new Set<unknown>();
  const failures = new Map(Object.entries(options.failTimes ?? {}));
  const downloads = new Map<string, DownloadItem>();
  const downloading = new Set<string>();
  let upcoming: TrackMetadata[] = [];
  let lastResults: TrackMetadata[] = [];
  let playGeneration = 0;
  let tick: unknown;
  let disposed = false;
  const snapshot: Snapshot = {
    revision: 0, track: null, position_ms: 0, duration_ms: 0, paused: true, volume: 1, wave: false, discovering: false,
    next_track: null, preload: 'idle', queue: [], error: null,
  };

  /** One-shot timer that forgets itself, so dispose() only has live handles to clear. */
  function later(ms: number, callback: () => void): void {
    if (disposed) return;
    const handle = timers.set(() => { pending.delete(handle); if (!disposed) callback(); }, ms);
    pending.add(handle);
  }
  const wait = (ms: number): Promise<void> => new Promise(resolve => later(ms, resolve));

  function publish(): void {
    snapshot.revision++;
    snapshot.next_track = upcoming[0] ?? null;
    snapshot.queue = upcoming.slice(1);
    snapshot.preload = snapshot.next_track ? 'ready' : 'idle';
    emit({ type: 'state', state: structuredCloneSnapshot(snapshot) });
  }
  /** Position ticks run only while something plays; paused or idle means no timer at all. */
  function syncTick(): void {
    const playing = snapshot.track !== null && !snapshot.paused && !disposed;
    if (playing && tick === undefined) tick = timers.set(onTick, TICK_MS);
    if (!playing && tick !== undefined) { timers.clear(tick); tick = undefined; }
  }
  function onTick(): void {
    tick = undefined;
    if (!snapshot.track || snapshot.paused || disposed) return;
    snapshot.position_ms = Math.min(snapshot.duration_ms, snapshot.position_ms + TICK_MS);
    if (snapshot.position_ms >= snapshot.duration_ms) { advance(); return; }
    publish();
    syncTick();
  }
  function start(next: TrackMetadata | null): void {
    snapshot.track = next;
    snapshot.position_ms = 0;
    snapshot.duration_ms = next?.duration_ms ?? 0;
    snapshot.paused = next === null;
    snapshot.error = null;
    publish();
    syncTick();
  }
  function advance(): void {
    playGeneration++; // a skip supersedes a play that is still buffering
    const next = upcoming.shift() ?? null;
    if (!next) snapshot.wave = false;
    start(next);
  }

  async function play(args: Record<string, unknown>): Promise<void> {
    const requested = args.track as TrackMetadata | undefined;
    if (!requested?.id) throw new Error('play_track: missing track');
    const generation = ++playGeneration;
    if (requested.id.startsWith('broken')) {
      await wait(300);
      if (generation === playGeneration) { snapshot.error = 'native decode failed'; publish(); }
      throw new Error('native decode failed');
    }
    await wait(500); // simulated stream resolution and buffering
    if (generation !== playGeneration) return; // superseded by a newer play or a skip
    const index = lastResults.findIndex(t => t.id === requested.id);
    // Like the real backend: playing a search result queues the results after it.
    upcoming = index >= 0 ? lastResults.slice(index + 1).filter(t => !t.id.startsWith('broken')) : upcoming.filter(t => t.id !== requested.id);
    snapshot.wave = false;
    start(requested);
  }

  function control(args: Record<string, unknown>): void {
    const action = String(args.action);
    const value = typeof args.value === 'number' ? args.value : Number.NaN;
    if (action === 'pause') snapshot.paused = true;
    else if (action === 'resume') snapshot.paused = snapshot.track === null;
    else if (action === 'skip') { advance(); return; }
    else if (action === 'seek') snapshot.position_ms = clampSeek(value, snapshot.duration_ms);
    else if (action === 'volume') { if (Number.isFinite(value)) snapshot.volume = Math.min(1, Math.max(0, value)); }
    else throw new Error(`Unknown action: ${action}`);
    publish();
    syncTick();
  }

  async function wave(): Promise<TrackMetadata[]> {
    snapshot.wave = true;
    snapshot.discovering = true;
    publish();
    await wait(800);
    snapshot.discovering = false;
    const known = new Set([snapshot.track?.id, ...upcoming.map(t => t.id)]);
    const fresh = demoCatalog.filter(t => !t.id.startsWith('broken') && !known.has(t.id)).slice(0, 4);
    upcoming = [...upcoming, ...fresh];
    if (snapshot.track) publish();
    else { playGeneration++; start(upcoming.shift() ?? null); }
    return fresh;
  }

  function download(args: Record<string, unknown>): void {
    const t = args.track as TrackMetadata | undefined;
    if (!t?.id) throw new Error('download_track: missing track');
    if (downloads.has(t.id)) throw new Error('Already downloaded');
    if (downloading.has(t.id)) throw new Error('Download already in progress');
    downloading.add(t.id);
    const total = bytesFor(t);
    const event = (status: 'started' | 'progress' | 'done', done: number): PlayerEvent =>
      ({ type: 'download', track_id: t.id, status, done_bytes: done, total_bytes: total, message: null });
    later(0, () => emit(event('started', 0)));
    later(300, () => emit(event('progress', Math.round(total * 0.4))));
    later(600, () => emit(event('progress', Math.round(total * 0.8))));
    later(900, () => {
      downloading.delete(t.id);
      downloads.set(t.id, { track: t, path: `Демо-папка/${t.title}.mp3`, bytes: total });
      emit(event('done', total));
    });
  }

  async function run(name: string, args: Record<string, unknown>): Promise<unknown> {
    const left = failures.get(name) ?? 0;
    if (left > 0) { failures.set(name, left - 1); await wait(300); throw new Error('transport error: connection reset (demo)'); }
    switch (name) {
      case 'get_player_state': return structuredCloneSnapshot(snapshot);
      case 'search_tracks': {
        await wait(400);
        const text = String(args.query ?? '').trim().toLowerCase();
        if (text === 'ошибка') throw new Error('Deezer: HTTP 429 (rate limited); retry in 30 seconds');
        lastResults = text ? demoCatalog.filter(t => `${t.title} ${t.artists.join(' ')}`.toLowerCase().includes(text)) : [];
        return lastResults.map(t => ({ ...t, artists: [...t.artists] }));
      }
      case 'play_track': return play(args);
      case 'control_playback': return control(args);
      case 'start_my_wave': return wave();
      case 'download_track': return download(args);
      case 'list_downloads': {
        await wait(300);
        const list: DownloadList = { dir: 'Демо-папка', items: [...downloads.values()] };
        return list;
      }
      case 'delete_download': {
        const id = String(args.trackId ?? '');
        if (!downloads.delete(id)) throw new Error('Not downloaded');
        emit({ type: 'download', track_id: id, status: 'removed', done_bytes: 0, total_bytes: null, message: null });
        return undefined;
      }
      default: throw new Error(`Unknown command: ${name}`);
    }
  }

  return {
    command<T>(name: string, args: Record<string, unknown> = {}): Promise<T> {
      if (disposed) return Promise.reject(new Error('Demo backend disposed'));
      return run(name, args) as Promise<T>;
    },
    dispose(): void {
      disposed = true;
      if (tick !== undefined) { timers.clear(tick); tick = undefined; }
      for (const handle of pending) timers.clear(handle);
      pending.clear();
    },
  };
}

function structuredCloneSnapshot(state: Snapshot): Snapshot {
  return JSON.parse(JSON.stringify(state)) as Snapshot;
}
