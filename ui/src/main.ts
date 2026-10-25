import { invoke, isTauri } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { DynamicTheme, safeCoverUrl } from './theme';
import './styles.css';

interface TrackMetadata {
  id: string;
  title: string;
  artists: string[];
  album: string | null;
  duration_ms: number;
  cover_url: string | null;
  isrc: string | null;
}
interface Snapshot {
  revision: number;
  track: TrackMetadata | null;
  position_ms: number;
  duration_ms: number;
  paused: boolean;
  volume: number;
  wave: boolean;
  next_track: TrackMetadata | null;
  preload: 'idle' | 'resolving' | 'buffering' | 'ready';
  queue: TrackMetadata[];
  error: string | null;
}
type PlayerEvent = { type: 'state'; state: Snapshot }
  | { type: 'gapless'; from_track_id: string; to_track_id: string }
  | { type: 'error'; message: string };

function element<T extends HTMLElement>(id: string): T {
  const node = document.getElementById(id);
  if (!node) throw new Error(`Missing element: ${id}`);
  return node as T;
}
function all<T extends HTMLElement>(selector: string): T[] {
  return Array.from(document.querySelectorAll<T>(selector));
}
const theme = new DynamicTheme();
const sheet = element<HTMLDialogElement>('player-sheet');
const query = element<HTMLInputElement>('query');
let searching = false;
const status = element('app-status');
const toast = element('toast');
let state: Snapshot | undefined;
let unlisten: UnlistenFn | undefined;
let toastTimer: ReturnType<typeof setTimeout> | undefined;
let results: TrackMetadata[] = [];
let visibleResults = 0;
let queueKey = '';
let artwork: string | null | undefined;
let activeId: string | null | undefined;
let waveBusy = false;
let playRequest = 0;
const noteIcon = document.querySelector('.cover-fallback')!.cloneNode(true);
const titles: Record<string, string> = { home: 'Главная', search: 'Поиск', queue: 'Очередь' };
const sourceValue = (): string => document.querySelector<HTMLInputElement>('input[name="source"]:checked')?.value ?? 'spotify';
const placeholders: Record<string, string> = {
  spotify: 'Трек, исполнитель или ссылка', soundcloud: 'Трек или ссылка SoundCloud',
  'spotify-playlist': 'Ссылка на плейлист Spotify', 'soundcloud-playlist': 'Ссылка на плейлист SoundCloud',
};
/** Paint the M3 active track; WebKit has no ::range-progress. */
function sliderFill(slider: HTMLInputElement): void {
  const max = Number(slider.max) || 1;
  slider.style.setProperty('--fill', `${(Number(slider.value) / max) * 100}%`);
}

const formatTime = (ms: number): string => {
  const seconds = Math.max(0, Math.floor(ms / 1000));
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, '0')}`;
};
/** Short Russian explanations for known backend errors; the raw text stays visible in app-status. */
const knownErrors: [RegExp, string][] = [
  [/HLS: (encrypted|master|live|byte-range)/, 'Этот поток SoundCloud защищён или устроен нестандартно. Ищем другой источник.'],
  [/native (HLS )?decode failed|Opus needs an external/, 'Встроенный декодер не справился с форматом этого трека. Попробуй другой трек или источник.'],
  [/HTTP 403/, 'Сервис отказал в доступе к потоку (403). Возможно, ссылка устарела — повтори воспроизведение.'],
  [/transport error|download failed|truncated range|timed out|timeout/i, 'Проблема с сетью при загрузке потока. Проверь подключение и повтори.'],
  [/no full-length|no match for/, 'Не нашли полную версию трека на YouTube или SoundCloud.'],
];
function errorText(message: unknown): string {
  const text = String(message);
  const limit = text.match(/Spotify: HTTP 429 \(rate limited\); retry in (\d+) seconds/);
  if (limit) {
    const minutes = Math.ceil(Number(limit[1]) / 60);
    const wait = minutes >= 60 ? `${Math.floor(minutes / 60)} ч ${minutes % 60} мин` : `${minutes} мин`;
    return `Spotify временно ограничил этот запрос (ещё ${wait}). Поиск и ссылки на треки работают отдельно — попробуй их.`;
  }
  return knownErrors.find(([pattern]) => pattern.test(text))?.[1] ?? text;
}
function notify(message: unknown): void {
  const text = errorText(message);
  status.textContent = text;
  toast.textContent = text;
  toast.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { toast.hidden = true; }, 6500);
}
async function command<T>(name: string, args?: Record<string, unknown>): Promise<T> {
  if (!isTauri()) throw new Error('В браузере доступен только интерфейс. Для музыки запусти приложение Tauri.');
  return invoke<T>(name, args);
}
function setCover(image: HTMLImageElement, cover: string | null): void {
  image.hidden = true;
  image.classList.remove('loaded');
  image.referrerPolicy = 'no-referrer';
  image.onload = () => { image.classList.add('loaded'); };
  image.onerror = () => { image.hidden = true; image.classList.remove('loaded'); };
  const url = safeCoverUrl(cover);
  // Keep a layout box so native lazy loading can observe the image.
  if (url) { image.hidden = false; image.src = url; }
  else image.removeAttribute('src');
}
async function play(track: TrackMetadata): Promise<void> {
  const request = ++playRequest;
  status.textContent = `Ищем поток: ${track.title}…`;
  try {
    await command<void>('play_track', { track });
    if (request === playRequest && state?.track?.id !== track.id && !state?.error) {
      status.textContent = `Поток найден. Буферизация: ${track.title}`;
    }
  } catch (error) { if (request === playRequest) notify(error); }
}
function trackRow(track: TrackMetadata): HTMLButtonElement {
  const button = document.createElement('button');
  button.type = 'button';
  button.className = 'track-row';
  button.dataset.trackId = track.id;
  button.classList.toggle('current', state?.track?.id === track.id);
  button.setAttribute('aria-label', `Воспроизвести ${track.title}, ${track.artists.join(', ')}`);
  const cover = document.createElement('span');
  cover.className = 'cover small';
  const image = document.createElement('img');
  image.alt = ''; image.loading = 'lazy'; image.decoding = 'async';
  setCover(image, track.cover_url);
  cover.append(image, noteIcon.cloneNode(true));
  const copy = document.createElement('span'); copy.className = 'track-copy';
  const title = document.createElement('strong'); title.textContent = track.title;
  const artists = document.createElement('span'); artists.textContent = track.artists.join(', ');
  copy.append(title, artists);
  const duration = document.createElement('span'); duration.className = 'track-length'; duration.textContent = formatTime(track.duration_ms);
  button.append(cover, copy, duration);
  button.addEventListener('click', () => { void play(track); });
  return button;
}
function appendResults(): void {
  const end = Math.min(visibleResults + 40, results.length);
  const fragment = document.createDocumentFragment();
  for (const track of results.slice(visibleResults, end)) fragment.append(trackRow(track));
  element('results').append(fragment);
  visibleResults = end;
  element('more-results').hidden = visibleResults >= results.length;
}
function render(next: Snapshot): void {
  // Subscribe before snapshot fetch; ignore a late older snapshot response.
  if (state && next.revision <= state.revision) return;
  state = next;
  const track = next.track;
  const title = track?.title ?? 'Музыка ждёт';
  const artists = track?.artists.join(', ') ?? 'Выбери свой первый трек';
  for (const id of ['bar-title', 'sheet-title']) element(id).textContent = title;
  for (const id of ['bar-artist', 'sheet-artist']) element(id).textContent = artists;
  const cover = track?.cover_url ?? null;
  if (artwork !== cover) {
    artwork = cover;
    setCover(element<HTMLImageElement>('bar-cover'), artwork);
    setCover(element<HTMLImageElement>('sheet-cover'), artwork);
    void theme.update(artwork);
  }
  document.body.classList.toggle('playing', Boolean(track && !next.paused));
  for (const button of all<HTMLButtonElement>('.play-toggle')) {
    button.disabled = !track;
    button.setAttribute('aria-label', next.paused || !track ? 'Воспроизвести' : 'Пауза');
  }
  element('mini-progress-bar').style.width = `${next.duration_ms ? Math.min(100, next.position_ms / next.duration_ms * 100) : 0}%`;
  for (const button of all<HTMLButtonElement>('.skip-track')) button.disabled = !track;
  for (const button of all<HTMLButtonElement>('.wave-toggle')) button.setAttribute('aria-pressed', String(next.wave));
  for (const node of all<HTMLElement>('.position')) node.textContent = formatTime(next.position_ms);
  for (const node of all<HTMLElement>('.duration')) node.textContent = formatTime(next.duration_ms);
  for (const slider of all<HTMLInputElement>('.seek')) {
    slider.disabled = !track || !next.duration_ms;
    slider.max = String(Math.max(1, next.duration_ms));
    if (slider.dataset.editing !== 'true') slider.value = String(Math.min(next.position_ms, next.duration_ms));
    slider.setAttribute('aria-valuetext', `${formatTime(Number(slider.value))} из ${formatTime(next.duration_ms)}`);
    sliderFill(slider);
  }
  for (const slider of all<HTMLInputElement>('.volume')) {
    if (slider.dataset.editing !== 'true') slider.value = String(next.volume);
    sliderFill(slider);
  }
  const preload = { idle: 'Очередь пуста', resolving: 'Ищем следующий поток…', buffering: 'Предзагрузка…', ready: 'Следующий готов' }[next.preload];
  for (const label of all<HTMLElement>('.gapless-status')) {
    label.textContent = next.preload === 'idle' && next.queue.length ? 'Следующий трек в очереди' : preload;
    label.dataset.ready = String(next.preload === 'ready');
  }
  element('next-title').textContent = next.next_track ? `${next.next_track.title} · ${next.next_track.artists.join(', ')}` : 'Следующий трек ещё не выбран';
  element('queue-mode').textContent = next.wave ? 'Моя Волна · автопополнение' : 'Обычная очередь';
  const queued = [...(next.next_track ? [next.next_track] : []), ...next.queue];
  const key = JSON.stringify(queued.map(t => t.id));
  if (key !== queueKey) {
    queueKey = key;
    element('queue-list').replaceChildren(...queued.map(trackRow));
    element('queue-empty').hidden = queued.length > 0;
  }
  if (activeId !== track?.id) {
    activeId = track?.id;
    for (const row of all<HTMLElement>('.track-row')) row.classList.toggle('current', row.dataset.trackId === activeId);
    if (track) status.textContent = `Играет: ${title} · ${artists}`;
  }
  if (next.error) status.textContent = errorText(next.error);
}

async function control(action: string, value?: number): Promise<void> {
  try { await command<void>('control_playback', { action, value: value ?? null }); }
  catch (error) { notify(error); }
}
async function startWave(): Promise<void> {
  if (waveBusy) return;
  waveBusy = true;
  const buttons = all<HTMLButtonElement>('.wave-start, .wave-toggle');
  buttons.forEach(button => { button.disabled = true; });
  try {
    await command<TrackMetadata[]>('start_my_wave');
    status.textContent = 'Моя Волна включена. Рекомендации обновляются по мере прослушивания.';
  } catch (error) { notify(error); }
  finally { waveBusy = false; buttons.forEach(button => { button.disabled = false; }); }
}

function show(view: string): void {
  for (const nav of all<HTMLButtonElement>('.nav-item')) {
    const active = nav.dataset.view === view;
    nav.classList.toggle('active', active);
    if (active) nav.setAttribute('aria-current', 'page'); else nav.removeAttribute('aria-current');
  }
  for (const name of Object.keys(titles)) element(`${name}-view`).hidden = name !== view;
  element('page-title').textContent = titles[view] ?? titles.home;
  window.scrollTo({ top: 0 });
  if (view === 'search') query.focus({ preventScroll: true });
}
for (const button of all<HTMLButtonElement>('.nav-item')) button.addEventListener('click', () => show(button.dataset.view ?? 'home'));
for (const card of all<HTMLButtonElement>('.action-card')) card.addEventListener('click', () => {
  const radio = document.querySelector<HTMLInputElement>(`input[name="source"][value="${card.dataset.source}"]`);
  if (radio) { radio.checked = true; query.placeholder = placeholders[radio.value]; }
  show(card.dataset.goto ?? 'search');
});
for (const radio of all<HTMLInputElement>('input[name="source"]')) radio.addEventListener('change', () => {
  query.placeholder = placeholders[radio.value];
  if (query.value.trim()) element<HTMLFormElement>('search-form').requestSubmit();
});
element<HTMLFormElement>('search-form').addEventListener('submit', async event => {
  event.preventDefault();
  if (searching || !query.value.trim()) return;
  searching = true;
  query.blur(); // hide the on-screen keyboard on Android
  element('search-progress').hidden = false;
  element('search-status').textContent = 'Ищем музыку…';
  try {
    results = await command<TrackMetadata[]>('search_tracks', { query: query.value.trim(), source: sourceValue() });
    visibleResults = 0;
    element('results').replaceChildren();
    appendResults();
    element('search-status').textContent = results.length ? `Найдено: ${results.length}. Треки добавлены в локальную библиотеку Волны.` : 'Ничего не найдено.';
  } catch (error) { element('search-status').textContent = 'Поиск не удался. Попробуй другой источник или прямую ссылку.'; notify(error); }
  finally { searching = false; element('search-progress').hidden = true; }
});
element('more-results').addEventListener('click', appendResults);
for (const button of all<HTMLButtonElement>('.wave-start, .wave-toggle')) button.addEventListener('click', () => { void startWave(); });
for (const button of all<HTMLButtonElement>('.play-toggle')) button.addEventListener('click', () => { void control(state?.paused ? 'resume' : 'pause'); });
for (const button of all<HTMLButtonElement>('.skip-track')) button.addEventListener('click', () => { void control('skip'); });
for (const slider of all<HTMLInputElement>('.seek, .volume')) {
  slider.addEventListener('input', () => {
    slider.dataset.editing = 'true';
    sliderFill(slider);
    if (slider.classList.contains('seek')) slider.setAttribute('aria-valuetext', formatTime(Number(slider.value)));
  });
  slider.addEventListener('change', () => {
    slider.dataset.editing = 'false';
    void control(slider.classList.contains('seek') ? 'seek' : 'volume', Number(slider.value));
  });
  slider.addEventListener('blur', () => { slider.dataset.editing = 'false'; });
  slider.addEventListener('pointercancel', () => { slider.dataset.editing = 'false'; });
}
element('open-player').addEventListener('click', () => { if (!sheet.open) sheet.showModal(); });
element('close-player').addEventListener('click', () => { sheet.close(); });
// Native dialog handles focus trapping, Escape and restoration of focus.
let touchStart: number | undefined;
const sheetHeader = sheet.querySelector<HTMLElement>('.sheet-header')!;
sheetHeader.addEventListener('touchstart', event => { touchStart = event.touches[0]?.clientY; }, { passive: true });
sheetHeader.addEventListener('touchend', event => {
  if (touchStart !== undefined && (event.changedTouches[0]?.clientY ?? touchStart) - touchStart > 80) sheet.close();
  touchStart = undefined;
}, { passive: true });
document.addEventListener('visibilitychange', () => { document.body.classList.toggle('background', document.hidden); });
document.addEventListener('keydown', event => {
  const target = event.target;
  if (event.code !== 'Space' || event.repeat || !(target instanceof HTMLElement)
    || target.closest('input, textarea, select, button, [contenteditable]')) return;
  if (state?.track) { event.preventDefault(); void control(state.paused ? 'resume' : 'pause'); }
});
window.addEventListener('pagehide', () => { unlisten?.(); unlisten = undefined; });

async function connect(): Promise<void> {
  if (!isTauri()) { status.textContent = 'Предпросмотр интерфейса. Воспроизведение и поиск доступны в приложении Tauri.'; return; }
  try {
    unlisten = await listen<PlayerEvent>('player-event', ({ payload }) => {
      if (payload.type === 'state') render(payload.state);
      else if (payload.type === 'error') notify(payload.message);
      else notify('Бесшовный переход · без вставленной тишины');
    });
    render(await command<Snapshot>('get_player_state'));
    if (!state?.error) status.textContent = 'Готово. История и профиль вкуса хранятся локально.';
  } catch (error) { notify(error); }
}
void connect();