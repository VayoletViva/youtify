import { createDemoBackend, demoCatalog, type Timers } from './demo.js';
import { errorText } from './format.js';
import type { PlayerEvent, Snapshot, TrackMetadata } from './types.js';

function assert(condition: boolean, message: string): void {
  if (!condition) throw new Error(message);
}

/** Hand-driven clock: nothing runs until advance(), so the test takes milliseconds of real time. */
class FakeTimers implements Timers {
  now = 0;
  private next = 1;
  readonly live = new Map<number, { at: number; callback: () => void }>();
  set(callback: () => void, ms: number): unknown {
    const id = this.next++;
    this.live.set(id, { at: this.now + ms, callback });
    return id;
  }
  clear(handle: unknown): void { this.live.delete(handle as number); }
  async advance(ms: number): Promise<void> {
    const end = this.now + ms;
    for (;;) {
      let due: [number, { at: number; callback: () => void }] | undefined;
      for (const entry of this.live) if (entry[1].at <= end && (!due || entry[1].at < due[1].at)) due = entry;
      if (!due) break;
      this.live.delete(due[0]);
      this.now = due[1].at;
      due[1].callback();
      await flush();
    }
    this.now = end;
    await flush();
  }
}
const flush = (): Promise<void> => new Promise(resolve => setTimeout(resolve, 0));

function setup(options = {}) {
  const timers = new FakeTimers();
  const events: PlayerEvent[] = [];
  const backend = createDemoBackend(event => events.push(event), timers, options);
  const states = (): Snapshot[] => events.flatMap(event => event.type === 'state' ? [event.state] : []);
  return { timers, events, backend, states, last: (): Snapshot | undefined => states().at(-1) };
}
async function settle<T>(promise: Promise<T>, timers: FakeTimers, ms: number): Promise<{ value?: T; error?: unknown }> {
  let outcome: { value?: T; error?: unknown } | undefined;
  promise.then(value => { outcome = { value }; }, error => { outcome = { error }; });
  await timers.advance(ms);
  assert(outcome !== undefined, `Promise settled within ${ms} ms`);
  return outcome!;
}

// Search: substring match, the rate-limit error and the empty result.
{
  const { backend, timers } = setup();
  const found = await settle(backend.command<TrackMetadata[]>('search_tracks', { query: 'Сати', source: 'spotify' }), timers, 400);
  assert((found.value?.length ?? 0) >= 1 && found.value![0].artists[0] === 'Эрик Сати', 'Search finds Satie');
  const failed = await settle(backend.command('search_tracks', { query: 'ошибка', source: 'deezer' }), timers, 400);
  assert(failed.error !== undefined && errorText(failed.error).startsWith('Deezer временно ограничил запросы (ещё 30 с)'), 'Error query maps to the Deezer rate-limit text');
  const none = await settle(backend.command<TrackMetadata[]>('search_tracks', { query: 'пусто', source: 'spotify' }), timers, 400);
  assert(none.value?.length === 0, 'No match returns an empty list');
  backend.dispose();
}

// A broken file rejects and leaves no track playing.
{
  const { backend, timers, last } = setup();
  const broken = { id: 'broken-x', title: 'x', artists: ['y'], album: null, duration_ms: 1000, cover_url: null, isrc: null };
  const outcome = await settle(backend.command('play_track', { track: broken }), timers, 300);
  assert(String(outcome.error).includes('native decode failed'), 'Broken track rejects with a decoder error');
  assert(last()?.track === null && last()?.error === 'native decode failed', 'Error snapshot without a track');
  backend.dispose();
}

// Play, then pause: progress stops and no timer stays alive while paused.
{
  const { backend, timers, states, last } = setup();
  const [first] = (await settle(backend.command<TrackMetadata[]>('search_tracks', { query: 'Дебюсси' }), timers, 400)).value!;
  await settle(backend.command('play_track', { track: first }), timers, 500);
  assert(last()?.track?.id === first.id && last()?.paused === false, 'Track plays after buffering');
  await timers.advance(1000);
  assert((last()?.position_ms ?? 0) >= 750, 'Position advances while playing');
  await settle(backend.command('control_playback', { action: 'pause', value: null }), timers, 0);
  const pausedAt = states().length;
  const position = last()!.position_ms;
  await timers.advance(600);
  assert(states().length === pausedAt && last()!.position_ms === position, 'No progress events after pause');
  assert(timers.live.size === 0, 'Nothing polls while paused');
  await settle(backend.command('control_playback', { action: 'seek', value: 10_000_000 }), timers, 0);
  assert(last()!.position_ms === first.duration_ms, 'Seek is clamped to the duration');
  await settle(backend.command('control_playback', { action: 'volume', value: 0.125 }), timers, 0);
  assert(last()!.volume === 0.125, 'Volume stores linear gain');
  const revisions = states().map(state => state.revision);
  assert(revisions.every((revision, index) => index === 0 || revision > revisions[index - 1]), 'Revisions only grow');
  backend.dispose();
}

// Five rapid play requests: only the last one wins.
{
  const { backend, timers, states, last } = setup();
  const all = demoCatalog.filter(track => !track.id.startsWith('broken'));
  assert(all.length >= 5, 'Enough demo tracks');
  // All five start before any resolves, as with rapid taps on "next".
  const plays = all.slice(0, 5).map(track => backend.command('play_track', { track }));
  await timers.advance(1000);
  await Promise.all(plays);
  const played = states().filter(state => state.track).map(state => state.track!.id);
  assert(last()?.track?.id === all[4].id, 'The fifth track is current');
  assert(played.every(id => id === all[4].id), 'No earlier track started over the last one');
  backend.dispose();
}

// Wave fills the queue; downloads go started -> progress -> done and then list and delete.
{
  const { backend, timers, events, last } = setup();
  await settle(backend.command('start_my_wave'), timers, 800);
  assert(last()?.discovering === false && last()?.wave === true && last()?.track !== null, 'Wave starts playing');
  assert(last()?.next_track !== null && (last()?.queue.length ?? 0) >= 1, 'Wave fills the queue');
  const track = last()!.track!;
  await settle(backend.command('download_track', { track }), timers, 0);
  await timers.advance(900);
  const statuses = events.flatMap(event => event.type === 'download' ? [event.status] : []);
  assert(statuses.join() === 'started,progress,progress,done', `Download events in order: ${statuses.join()}`);
  const again = await settle(backend.command('download_track', { track }), timers, 0);
  assert(String(again.error).includes('Already downloaded'), 'Second download is refused');
  const list = await settle(backend.command<{ dir: string; items: unknown[] }>('list_downloads'), timers, 300);
  assert(list.value?.items.length === 1 && list.value.dir === 'Демо-папка', 'Downloads list the track');
  await settle(backend.command('delete_download', { trackId: track.id }), timers, 0);
  assert(events.at(-1)?.type === 'download' && (events.at(-1) as { status: string }).status === 'removed', 'Delete emits removed');
  backend.dispose();
}

// failTimes: list_downloads fails twice (start-up and opening the view), the retry works.
{
  const { backend, timers } = setup({ failTimes: { list_downloads: 2 } });
  const first = await settle(backend.command('list_downloads'), timers, 300);
  assert(errorText(first.error).startsWith('Проблема с сетью'), 'First list fails with a network error');
  const second = await settle(backend.command('list_downloads'), timers, 300);
  assert(second.error !== undefined, 'Second list (opening the view) fails too');
  const third = await settle(backend.command('list_downloads'), timers, 300);
  assert(third.value !== undefined, 'Retry succeeds');
  backend.dispose();
}

// dispose() clears every timer, including the playback tick.
{
  const { backend, timers } = setup();
  const [track] = (await settle(backend.command<TrackMetadata[]>('search_tracks', { query: 'Григ' }), timers, 400)).value!;
  await settle(backend.command('play_track', { track }), timers, 500);
  void backend.command('download_track', { track });
  assert(timers.live.size > 0, 'Timers are running before dispose');
  backend.dispose();
  assert(timers.live.size === 0, 'dispose() clears all timers');
  const rejected = await settle(backend.command('get_player_state'), timers, 0);
  assert(rejected.error !== undefined, 'Commands after dispose reject');
}
console.log('Demo backend assertions passed');
