// M3 typeface, bundled (the Tauri CSP allows fonts only from 'self'). Before styles.css so @font-face comes first.
import '@fontsource-variable/roboto-flex';
import { invoke, isTauri } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { createDemoBackend } from './demo';
import { clampSeek, downloadPercent, errorText, formatSize, formatTime, searchEmptyText, sourceLabels } from './format';
import { RequestGate } from './requests';
import { DynamicTheme, safeCoverUrl } from './theme';
import type { DownloadEvent, DownloadList, PlayerEvent, Snapshot, TrackMetadata } from './types';
import { gainToSlider, sliderToGain, volumeLabel } from './volume';
import './styles.css';

function element<T extends HTMLElement>(id: string): T {
  const node = document.getElementById(id);
  if (!node) throw new Error(`Missing element: ${id}`);
  return node as T;
}
function all<T extends HTMLElement>(selector: string): T[] {
  return Array.from(document.querySelectorAll<T>(selector));
}
/** Browser preview only (never inside Tauri): ?demo swaps the Tauri bridge for an in-memory backend.
 *  ?demo=downloads-error makes list_downloads fail at start-up and on opening the view (retry succeeds), to show the error state. */
const demoMode = isTauri() ? null : new URLSearchParams(location.search).get('demo');
const demo = demoMode === null ? undefined
  : createDemoBackend(payload => handleEvent(payload), undefined, { failTimes: demoMode === 'downloads-error' ? { list_downloads: 2 } : {} });
const hasBackend = isTauri() || demo !== undefined;

const theme = new DynamicTheme();
const sheet = element<HTMLDialogElement>('player-sheet');
const query = element<HTMLInputElement>('query');
const searchForm = element<HTMLFormElement>('search-form');
const resultsList = element('results');
const status = element('app-status');
const statusText = element('app-status-text');
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
/** Latest-wins gates: a slower, older response never overwrites a newer one. */
const searchGate = new RequestGate();
const playGate = new RequestGate();
const downloadsGate = new RequestGate();
/** The row whose stream is being resolved; re-applied when lists are rebuilt. */
let pendingPlayId: string | null = null;
/** Download state shared by every row and the sheet; list_downloads is the source of truth, events patch it. */
const downloaded = new Set<string>();
const downloading = new Map<string, { done: number; total: number | null }>();
const trackTitles = new Map<string, string>();
let downloadsLoaded = false;
let downloadsTimer: ReturnType<typeof setTimeout> | undefined;
const noteIcon = document.querySelector('.cover-fallback')!.cloneNode(true);
const titles: Record<string, string> = { home: 'Главная', search: 'Поиск', queue: 'Очередь', downloads: 'Загрузки' };
const waveCopy = element('wave-supporting').textContent ?? '';
const waveLabel = document.querySelector<HTMLElement>('.wave-start .wave-label')!;
const waveLabelText = waveLabel.textContent ?? '';
const sourceValue = (): string => document.querySelector<HTMLInputElement>('input[name="source"]:checked')?.value ?? 'spotify';
const placeholders: Record<string, string> = {
  spotify: 'Трек, исполнитель или ссылка', deezer: 'Трек, исполнитель или ссылка Deezer', soundcloud: 'Трек или ссылка SoundCloud',
  'spotify-playlist': 'Ссылка на плейлист Spotify', 'deezer-playlist': 'Ссылка на плейлист Deezer',
  'soundcloud-playlist': 'Ссылка на плейлист SoundCloud',
};
const icon = (path: string): string => `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="${path}"/></svg>`;
// pathLength=100 lets the dash offset be the percentage directly.
const ring = (extra = ''): string => `<svg class="ring${extra}" viewBox="0 0 24 24" aria-hidden="true"><circle class="ring-track" cx="12" cy="12" r="8.5" pathLength="100"/><circle class="ring-value" cx="12" cy="12" r="8.5" pathLength="100"/></svg>`;
const icons = {
  download: icon('M5 20h14v-2H5v2Zm14-9h-4V3H9v8H5l7 7 7-7Z'),
  done: icon('M5 18h14v2H5v-2Zm4.6-2.7L5 10.7l1.9-1.9 2.7 2.6L17.1 4 19 5.9l-9.4 9.4Z'),
  remove: icon('M6 19c0 1.1.9 2 2 2h8a2 2 0 0 0 2-2V7H6v12ZM19 4h-3.5l-1-1h-5l-1 1H5v2h14V4Z'),
  ring: ring(),
};
/** Paint the M3 active track; WebKit has no ::range-progress. */
function sliderFill(slider: HTMLInputElement): void {
  const max = Number(slider.max) || 1;
  slider.style.setProperty('--fill', `${(Number(slider.value) / max) * 100}%`);
}

/** The status line under the views; errors get an icon and data-kind="error" (never color alone). */
function setStatus(text: string, error = false): void {
  statusText.textContent = text;
  status.dataset.kind = error ? 'error' : 'info';
}
/** Short explanation plus the raw backend detail (codec, source), which stays visible for bug reports. */
function detail(message: unknown): string {
  const text = errorText(message);
  const raw = String(message);
  return text === raw ? text : `${text} (${raw})`;
}
function notify(message: unknown, error = false): void {
  setStatus(detail(message), error);
  toast.textContent = errorText(message);
  toast.classList.remove('leaving');
  toast.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    toast.classList.add('leaving');
    afterExit(toast, () => { if (toast.classList.contains('leaving')) { toast.hidden = true; toast.classList.remove('leaving'); } });
  }, 6500);
}
const fail = (error: unknown): void => notify(error, true);
async function command<T>(name: string, args?: Record<string, unknown>): Promise<T> {
  if (demo) return demo.command<T>(name, args);
  if (!isTauri()) throw new Error('В браузере доступен только интерфейс. Для музыки запусти приложение Tauri.');
  return invoke<T>(name, args);
}
function setCover(image: HTMLImageElement, cover: string | null): void {
  // Crossfade: keep the old artwork under the new one until it has loaded.
  const box = image.parentElement;
  const old = image.classList.contains('loaded') && image.currentSrc;
  if (box) box.style.backgroundImage = old ? `url("${old.replace(/["\\]/g, '\\$&')}")` : '';
  image.hidden = true;
  image.classList.remove('loaded');
  image.referrerPolicy = 'no-referrer';
  const clearOld = (): void => { if (box) box.style.backgroundImage = ''; };
  image.onload = () => { image.classList.add('loaded'); setTimeout(clearOld, 550); };
  image.onerror = () => { image.hidden = true; image.classList.remove('loaded'); clearOld(); };
  const url = safeCoverUrl(cover);
  // Keep a layout box so native lazy loading can observe the image.
  if (url) { image.hidden = false; image.src = url; }
  else { image.removeAttribute('src'); clearOld(); }
}
/** Subtle staggered entrance; only the first rows get a delay so long lists don't keep animating. */
function enter(rows: HTMLElement[]): void {
  rows.forEach((row, index) => { row.classList.add('enter'); row.style.setProperty('--i', String(Math.min(index, 10))); });
}
/** Mark the row whose stream is resolving (ring instead of the duration); null clears every mark. */
function markPending(id: string | null): void {
  pendingPlayId = id;
  for (const row of all<HTMLElement>('.track-item')) paintPending(row);
}
function paintPending(row: HTMLElement): void {
  const on = pendingPlayId !== null && row.dataset.trackId === pendingPlayId;
  row.classList.toggle('pending', on);
  if (on) row.setAttribute('aria-busy', 'true'); else row.removeAttribute('aria-busy');
}
async function play(track: TrackMetadata): Promise<void> {
  const token = playGate.begin();
  markPending(track.id);
  setStatus(`Ищем поток: ${track.title}…`);
  try {
    await command<void>('play_track', { track });
    if (!playGate.isCurrent(token)) return;
    if (state?.track?.id === track.id) markPending(null);
    else if (!state?.error) setStatus(`Поток найден. Буферизация: ${track.title}`);
  } catch (error) {
    if (!playGate.isCurrent(token)) return;
    markPending(null);
    fail(error);
  }
}

/** Sync one trailing download button (row or sheet) with the shared state; rebuilds the icon only when its kind changes. */
function paintDownload(button: HTMLButtonElement): void {
  const id = button.dataset.downloadId;
  const title = button.dataset.title ?? '';
  const progress = id ? downloading.get(id) : undefined;
  const kind = progress ? 'progress' : id && downloaded.has(id) ? 'done' : 'idle';
  button.disabled = !id;
  if (button.dataset.kind !== kind) {
    button.dataset.kind = kind;
    button.innerHTML = kind === 'progress' ? icons.ring : kind === 'done' ? icons.done : icons.download;
  }
  if (progress) {
    const percent = downloadPercent(progress.done, progress.total);
    button.querySelector('.ring')?.classList.toggle('indeterminate', percent === null);
    const value = button.querySelector<SVGCircleElement>('.ring-value');
    if (value) value.style.strokeDashoffset = String(100 - (percent ?? 25));
    button.title = percent === null ? `Скачано ${formatSize(progress.done)}` : `${percent}%`;
    button.setAttribute('aria-label', percent === null ? `Скачивается: ${title}` : `Скачивается ${percent}%: ${title}`);
  } else {
    button.title = kind === 'done' ? 'Скачано, доступно офлайн' : 'Скачать';
    button.setAttribute('aria-label', !id ? 'Скачать' : kind === 'done' ? `Скачано: ${title}` : `Скачать ${title}`);
  }
}
function paintDownloads(id?: string): void {
  const selector = id ? `[data-download-id="${CSS.escape(id)}"]` : '[data-download-id]';
  for (const button of all<HTMLButtonElement>(selector)) paintDownload(button);
}
async function download(track: TrackMetadata): Promise<void> {
  if (downloading.has(track.id)) { notify('Download already in progress'); return; }
  if (downloaded.has(track.id)) { notify('Трек уже скачан и играет без сети. Удалить его можно в разделе «Загрузки».'); return; }
  trackTitles.set(track.id, track.title);
  // Optimistic: show the indeterminate ring until the first "started"/"progress" event.
  downloading.set(track.id, { done: 0, total: null });
  paintDownloads(track.id);
  try {
    await command<void>('download_track', { track });
    if (downloading.has(track.id)) setStatus(`Скачиваем: ${track.title}`);
  } catch (error) {
    downloading.delete(track.id);
    paintDownloads(track.id);
    fail(error);
    if (/Already downloaded/.test(String(error))) scheduleDownloadsRefresh();
  }
}
function downloadButton(track: TrackMetadata): HTMLButtonElement {
  const button = document.createElement('button');
  button.type = 'button';
  button.className = 'icon-button track-action';
  button.dataset.downloadId = track.id;
  button.dataset.title = track.title;
  paintDownload(button);
  button.addEventListener('click', () => { void download(track); });
  return button;
}
/** Delete asks for a second tap within 4 s instead of a modal; the button turns into the error color meanwhile. */
function deleteButton(track: TrackMetadata): HTMLButtonElement {
  const button = document.createElement('button');
  button.type = 'button';
  button.className = 'icon-button track-action delete-action';
  button.innerHTML = icons.remove;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const reset = (): void => {
    button.dataset.confirm = 'false';
    button.title = 'Удалить';
    button.setAttribute('aria-label', `Удалить ${track.title}`);
  };
  reset();
  button.addEventListener('click', async () => {
    clearTimeout(timer);
    if (button.dataset.confirm !== 'true') {
      button.dataset.confirm = 'true';
      button.title = 'Нажми ещё раз, чтобы удалить';
      button.setAttribute('aria-label', `Подтвердить удаление: ${track.title}`);
      notify(`Нажми ещё раз, чтобы удалить «${track.title}»`);
      timer = setTimeout(reset, 4000);
      return;
    }
    button.disabled = true;
    try {
      await command<void>('delete_download', { trackId: track.id });
      downloaded.delete(track.id);
      paintDownloads(track.id);
      notify(`Удалено: ${track.title}`);
      await refreshDownloads();
    } catch (error) { button.disabled = false; reset(); fail(error); }
  });
  return button;
}
/** A list item holding the play button and a trailing action; two siblings, since buttons can't nest. */
function trackRow(track: TrackMetadata, options: { meta?: string; remove?: boolean } = {}): HTMLDivElement {
  trackTitles.set(track.id, track.title);
  const item = document.createElement('div');
  item.className = 'track-item';
  item.setAttribute('role', 'listitem');
  item.dataset.trackId = track.id;
  item.classList.toggle('current', state?.track?.id === track.id);
  const button = document.createElement('button');
  button.type = 'button';
  button.className = 'track-row';
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
  const meta = document.createElement('span'); meta.className = 'track-length'; meta.textContent = options.meta ?? formatTime(track.duration_ms);
  button.append(cover, copy, meta);
  button.insertAdjacentHTML('beforeend', ring(' indeterminate row-ring'));
  button.addEventListener('click', () => { void play(track); });
  item.append(button, options.remove ? deleteButton(track) : downloadButton(track));
  paintPending(item);
  return item;
}
function appendResults(): void {
  const end = Math.min(visibleResults + 40, results.length);
  const rows = results.slice(visibleResults, end).map(track => trackRow(track));
  enter(rows);
  resultsList.append(...rows);
  visibleResults = end;
  element('more-results').hidden = visibleResults >= results.length;
}
function renderDownloads(list: DownloadList): void {
  const host = element('downloads-list');
  // Animate only newly listed files, so refreshes after each event don't flash.
  const known = new Set(all<HTMLElement>('#downloads-list .track-item').map(row => row.dataset.trackId));
  const rows = list.items.map(item => trackRow(item.track, { meta: formatSize(item.bytes), remove: true }));
  enter(rows.filter(row => !known.has(row.dataset.trackId)));
  host.replaceChildren(...rows);
  element('downloads-empty').hidden = list.items.length > 0;
  const total = list.items.reduce((sum, item) => sum + item.bytes, 0);
  element('downloads-status').textContent = list.items.length
    ? `${list.items.length} · ${formatSize(total)} · Папка: ${list.dir}`
    : `Папка: ${list.dir}`;
}
/** visible: the user is looking (view opened, retry), so show the linear progress; background refreshes stay quiet. */
async function refreshDownloads(visible = false): Promise<void> {
  if (!hasBackend) return;
  const token = downloadsGate.begin();
  const progress = element('downloads-progress');
  if (visible || !downloadsLoaded) progress.hidden = false;
  element('downloads-list').setAttribute('aria-busy', 'true');
  try {
    const list = await command<DownloadList>('list_downloads');
    if (!downloadsGate.isCurrent(token)) return;
    downloadsLoaded = true;
    element('downloads-error').hidden = true;
    downloaded.clear();
    for (const item of list.items) { downloaded.add(item.track.id); trackTitles.set(item.track.id, item.track.title); }
    renderDownloads(list);
    paintDownloads();
  } catch (error) {
    if (!downloadsGate.isCurrent(token)) return;
    element('downloads-status').textContent = 'Список загрузок сейчас недоступен.';
    element('downloads-error-text').textContent = errorText(error);
    element('downloads-error').hidden = false;
    element('downloads-empty').hidden = true;
    setStatus(detail(error), true);
  } finally {
    if (downloadsGate.isCurrent(token)) { progress.hidden = true; element('downloads-list').removeAttribute('aria-busy'); }
  }
}
/** Coalesce bursts (several downloads finishing together) into one list_downloads call. */
function scheduleDownloadsRefresh(): void {
  clearTimeout(downloadsTimer);
  downloadsTimer = setTimeout(() => { void refreshDownloads(); }, 250);
}
function onDownload(event: DownloadEvent): void {
  const id = event.track_id;
  const title = trackTitles.get(id) ?? 'трек';
  if (event.status === 'started' || event.status === 'progress') {
    downloading.set(id, { done: event.done_bytes, total: event.total_bytes });
  } else {
    downloading.delete(id);
    if (event.status === 'done') { downloaded.add(id); notify(`Скачано: ${title}. Теперь играет без сети.`); }
    else if (event.status === 'removed') downloaded.delete(id);
    else {
      const message = event.message ?? '';
      fail(message.startsWith('Download failed') ? message : `Download failed: ${message}`);
    }
    scheduleDownloadsRefresh();
  }
  paintDownloads(id);
}
/** The same handler serves the Tauri 'player-event' listener and the browser demo backend. */
function handleEvent(payload: PlayerEvent): void {
  if (payload.type === 'state') render(payload.state);
  else if (payload.type === 'error') fail(payload.message);
  else if (payload.type === 'download') onDownload(payload);
  else notify('Бесшовный переход · без вставленной тишины');
}

/** Wave buttons: a ring replaces the icon while the wave starts or fetches tracks; disabled only during the start. */
function paintWave(): void {
  // Older backends omit the field; treat it as idle.
  const discovering = state?.discovering === true;
  const busy = waveBusy || discovering;
  for (const button of all<HTMLButtonElement>('.wave-toggle, .wave-start, #queue-wave')) {
    if (button.classList.contains('wave-toggle')) button.setAttribute('aria-pressed', String(state?.wave === true));
    button.disabled = waveBusy;
    button.setAttribute('aria-busy', String(busy));
  }
  waveLabel.textContent = busy ? 'Ищем треки…' : waveLabelText;
  element('wave-supporting').textContent = discovering ? 'Ищем новые треки…' : waveCopy;
  element('queue-progress').hidden = !busy;
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
  // The pending row is done once its track is current, or when the backend reports an error.
  if (pendingPlayId !== null && (track?.id === pendingPlayId || next.error)) markPending(null);
  document.body.classList.toggle('playing', Boolean(track && !next.paused));
  for (const button of all<HTMLButtonElement>('.play-toggle')) {
    button.disabled = !track;
    button.setAttribute('aria-label', next.paused || !track ? 'Воспроизвести' : 'Пауза');
  }
  element('mini-progress-bar').style.transform = `scaleX(${next.duration_ms ? Math.min(1, next.position_ms / next.duration_ms) : 0})`;
  for (const button of all<HTMLButtonElement>('.skip-track')) button.disabled = !track;
  const discovering = next.discovering === true;
  paintWave();
  for (const node of all<HTMLElement>('.position')) node.textContent = formatTime(next.position_ms);
  for (const node of all<HTMLElement>('.duration')) node.textContent = formatTime(next.duration_ms);
  for (const slider of all<HTMLInputElement>('.seek')) {
    slider.disabled = !track || !next.duration_ms;
    slider.max = String(Math.max(1, next.duration_ms));
    if (slider.dataset.editing !== 'true') slider.value = String(clampSeek(next.position_ms, next.duration_ms));
    slider.setAttribute('aria-valuetext', `${formatTime(Number(slider.value))} из ${formatTime(next.duration_ms)}`);
    sliderFill(slider);
  }
  // The snapshot carries linear gain; the sliders hold the perceptual position.
  for (const slider of all<HTMLInputElement>('.volume')) {
    if (slider.dataset.editing !== 'true') slider.value = String(gainToSlider(next.volume));
    slider.setAttribute('aria-valuetext', volumeLabel(Number(slider.value)));
    sliderFill(slider);
  }
  const preload = { idle: 'Очередь пуста', resolving: 'Ищем следующий поток…', buffering: 'Предзагрузка…', ready: 'Следующий готов' }[next.preload];
  for (const label of all<HTMLElement>('.gapless-status')) {
    label.textContent = next.preload === 'idle' && next.queue.length ? 'Следующий трек в очереди' : preload;
    label.dataset.ready = String(next.preload === 'ready');
  }
  element('next-title').textContent = next.next_track ? `${next.next_track.title} · ${next.next_track.artists.join(', ')}` : 'Следующий трек ещё не выбран';
  element('queue-mode').textContent = discovering ? 'Моя Волна · Ищем новые треки…' : next.wave ? 'Моя Волна · автопополнение' : 'Обычная очередь';
  const queued = [...(next.next_track ? [next.next_track] : []), ...next.queue];
  const key = JSON.stringify(queued.map(t => t.id));
  if (key !== queueKey) {
    queueKey = key;
    // Animate only tracks that were not already listed, so queue refreshes don't flash.
    const known = new Set(all<HTMLElement>('#queue-list .track-item').map(row => row.dataset.trackId));
    const rows = queued.map(track => trackRow(track));
    enter(rows.filter(row => !known.has(row.dataset.trackId)));
    element('queue-list').replaceChildren(...rows);
    element('queue-empty').hidden = queued.length > 0;
  }
  if (activeId !== track?.id) {
    activeId = track?.id;
    for (const row of all<HTMLElement>('.track-item')) row.classList.toggle('current', row.dataset.trackId === activeId);
    const sheetDownload = element<HTMLButtonElement>('sheet-download');
    if (track) { sheetDownload.dataset.downloadId = track.id; sheetDownload.dataset.title = track.title; trackTitles.set(track.id, track.title); }
    else { delete sheetDownload.dataset.downloadId; delete sheetDownload.dataset.title; }
    paintDownload(sheetDownload);
    if (track) setStatus(`Играет: ${title} · ${artists}`);
  }
  if (next.error) setStatus(errorText(next.error), true);
}

async function control(action: string, value?: number): Promise<void> {
  try { await command<void>('control_playback', { action, value: value ?? null }); }
  catch (error) { fail(error); }
}
/** Live volume while dragging: one request in flight; on completion send the newest value if it moved.
 *  The release ('change') always sends, so the final value lands even if it equals the last live one.
 *  Values here are linear gain (sliderToGain of the slider position). */
let volumeBusy = false;
let volumePending: number | undefined;
let volumeCommit = false;
let volumeSent: number | undefined;
function setVolume(gain: number, commit: boolean): void {
  volumePending = gain;
  volumeCommit ||= commit;
  if (!volumeBusy) void flushVolume();
}
async function flushVolume(): Promise<void> {
  volumeBusy = true;
  try {
    while (volumePending !== undefined) {
      const value = volumePending, commit = volumeCommit;
      volumePending = undefined; volumeCommit = false;
      if (value === volumeSent && !commit) continue;
      volumeSent = value;
      // Live ticks fail silently (e.g. browser preview); the release reports the error once.
      try { await command<void>('control_playback', { action: 'volume', value }); }
      catch (error) { volumeSent = undefined; if (commit) fail(error); }
    }
  } finally { volumeBusy = false; }
}
async function startWave(): Promise<void> {
  if (waveBusy) return;
  waveBusy = true;
  paintWave();
  try {
    await command<TrackMetadata[]>('start_my_wave');
    setStatus('Моя Волна включена. Подмешиваем новые треки из сети и учимся по мере прослушивания.');
  } catch (error) { fail(error); }
  finally { waveBusy = false; paintWave(); }
}

const finePointer = matchMedia('(pointer: fine)');
const reducedMotion = matchMedia('(prefers-reduced-motion: reduce)');
const viewport = window.visualViewport;
/** Android WebView can stay panned/shrunk after the soft keyboard closes; fixed bars then sit below the
 *  visible edge. Pin them to the visual viewport (--vv-gap) and undo leftover pan once nothing is typing. */
function settleViewport(): void {
  if (!viewport) return;
  const typing = document.activeElement === query;
  // Desktop: innerHeight counts a horizontal scrollbar, so never offset there.
  const zoomed = viewport.scale > 1.01 || finePointer.matches;
  const gap = typing || zoomed ? 0 : Math.max(0, Math.round(window.innerHeight - viewport.height - viewport.offsetTop));
  document.documentElement.style.setProperty('--vv-gap', `${gap}px`);
  if (typing || zoomed) return;
  // Re-align the visual viewport with the layout viewport, undo any sideways pan (the page never scrolls
  // horizontally) and clamp overscroll left by the keyboard.
  const max = Math.max(0, document.documentElement.scrollHeight - window.innerHeight);
  const top = Math.min(Math.max(0, viewport.pageTop - viewport.offsetTop), max);
  if (viewport.offsetTop > 0.5 || viewport.offsetLeft > 0.5 || window.scrollX || window.scrollY > max) window.scrollTo(0, top);
}
viewport?.addEventListener('resize', settleViewport);
viewport?.addEventListener('scroll', settleViewport);
window.addEventListener('resize', settleViewport);
// The keyboard animates out after blur; settle again once it is gone.
query.addEventListener('blur', () => { requestAnimationFrame(settleViewport); setTimeout(settleViewport, 350); });
/** Run `done` once the element's exit animation ends; immediate with reduced motion or no animation. */
function afterExit(node: HTMLElement, done: () => void): void {
  const animations = reducedMotion.matches ? [] : node.getAnimations();
  if (!animations.length) { done(); return; }
  void Promise.all(animations.map(animation => animation.finished)).then(done, done);
}

function show(view: string): void {
  // Close the keyboard before its view disappears; a hidden focused input leaves the WebView panned.
  const focused = document.activeElement;
  if (focused instanceof HTMLElement && focused.closest('section[id$="-view"]') && !element(`${view}-view`).contains(focused)) focused.blur();
  for (const nav of all<HTMLButtonElement>('.nav-item')) {
    const active = nav.dataset.view === view;
    nav.classList.toggle('active', active);
    if (active) nav.setAttribute('aria-current', 'page'); else nav.removeAttribute('aria-current');
  }
  for (const name of Object.keys(titles)) element(`${name}-view`).hidden = name !== view;
  element('page-title').textContent = titles[view] ?? titles.home;
  if (window.scrollY || window.scrollX) window.scrollTo(0, 0); // instant: the new view fades in from the top
  settleViewport();
  if (view === 'downloads') void refreshDownloads(true);
  // Auto-focus only with a mouse; on touch it pops the soft keyboard on every visit.
  if (view === 'search' && finePointer.matches) query.focus({ preventScroll: true });
}
for (const button of all<HTMLButtonElement>('.nav-item')) button.addEventListener('click', () => show(button.dataset.view ?? 'home'));
for (const card of all<HTMLButtonElement>('.action-card')) card.addEventListener('click', () => {
  const radio = document.querySelector<HTMLInputElement>(`input[name="source"][value="${card.dataset.source}"]`);
  if (radio) { radio.checked = true; query.placeholder = placeholders[radio.value]; }
  show(card.dataset.goto ?? 'search');
});
// A chip change re-runs the search; the gate makes the newest source win even if a search is in flight.
for (const radio of all<HTMLInputElement>('input[name="source"]')) radio.addEventListener('change', () => {
  query.placeholder = placeholders[radio.value];
  if (query.value.trim()) searchForm.requestSubmit();
});
searchForm.addEventListener('submit', async event => {
  event.preventDefault();
  const text = query.value.trim();
  if (!text) return;
  const source = sourceValue();
  const token = searchGate.begin();
  if (document.activeElement === query) query.blur(); // hide the on-screen keyboard on Android
  const line = element('search-status');
  element('search-empty').hidden = true;
  element('search-error').hidden = true;
  line.hidden = false;
  line.textContent = 'Ищем музыку…';
  element('search-progress').hidden = false;
  resultsList.setAttribute('aria-busy', 'true');
  try {
    const found = await command<TrackMetadata[]>('search_tracks', { query: text, source });
    if (!searchGate.isCurrent(token)) return;
    results = found;
    visibleResults = 0;
    resultsList.replaceChildren();
    appendResults();
    if (found.length) line.textContent = `Найдено: ${found.length}. Треки добавлены в локальную библиотеку Волны.`;
    else {
      line.hidden = true;
      element('search-empty-text').textContent = searchEmptyText(text, sourceLabels[source] ?? source);
      element('search-empty').hidden = false;
    }
  } catch (error) {
    if (!searchGate.isCurrent(token)) return;
    results = [];
    visibleResults = 0;
    resultsList.replaceChildren();
    element('more-results').hidden = true;
    line.hidden = true;
    element('search-error-text').textContent = errorText(error);
    element('search-error').hidden = false;
    setStatus(detail(error), true);
  } finally {
    if (searchGate.isCurrent(token)) { element('search-progress').hidden = true; resultsList.removeAttribute('aria-busy'); }
  }
});
element('search-retry').addEventListener('click', () => {
  // The retry button hides with its block; keep keyboard focus in the search bar.
  searchForm.querySelector<HTMLButtonElement>('.search-submit')?.focus({ preventScroll: true });
  searchForm.requestSubmit();
});
element('search-change-source').addEventListener('click', () => {
  const next = document.querySelector<HTMLInputElement>('input[name="source"]:not(:checked)');
  if (!next) return;
  next.closest('label')?.scrollIntoView({ block: 'nearest', inline: 'nearest' });
  // focusVisible: show the ring even after a mouse click (ignored where unsupported).
  next.focus({ preventScroll: true, focusVisible: true } as FocusOptions);
});
element('more-results').addEventListener('click', appendResults);
for (const button of all<HTMLButtonElement>('.wave-start, .wave-toggle, #queue-wave')) button.addEventListener('click', () => { void startWave(); });
element('queue-search').addEventListener('click', () => show('search'));
element('downloads-retry').addEventListener('click', () => { void refreshDownloads(true); });
element('local-chip').addEventListener('click', () => notify('История и профиль вкуса хранятся только на этом устройстве.'));
for (const button of all<HTMLButtonElement>('.play-toggle')) button.addEventListener('click', () => { void control(state?.paused ? 'resume' : 'pause'); });
for (const button of all<HTMLButtonElement>('.skip-track')) button.addEventListener('click', () => {
  // A skip supersedes a play that is still resolving.
  playGate.cancel();
  markPending(null);
  void control('skip');
});
element('sheet-download').addEventListener('click', () => { if (state?.track) void download(state.track); });
paintDownload(element<HTMLButtonElement>('sheet-download'));
const volumeSliders = all<HTMLInputElement>('.volume');
const endVolumeEdit = (): void => { for (const other of volumeSliders) other.dataset.editing = 'false'; };
for (const slider of volumeSliders) slider.setAttribute('aria-valuetext', volumeLabel(Number(slider.value)));
for (const slider of all<HTMLInputElement>('.seek, .volume')) {
  const volume = slider.classList.contains('volume');
  slider.addEventListener('input', () => {
    if (volume) {
      // Mirror into the other volume slider so the mini player and the sheet agree mid-drag.
      for (const other of volumeSliders) {
        other.dataset.editing = 'true'; other.value = slider.value; sliderFill(other);
        other.setAttribute('aria-valuetext', volumeLabel(Number(slider.value)));
      }
      setVolume(sliderToGain(Number(slider.value)), false);
      return;
    }
    slider.dataset.editing = 'true';
    sliderFill(slider);
    slider.setAttribute('aria-valuetext', formatTime(Number(slider.value)));
  });
  slider.addEventListener('change', () => {
    if (volume) { endVolumeEdit(); setVolume(sliderToGain(Number(slider.value)), true); return; }
    slider.dataset.editing = 'false';
    void control('seek', clampSeek(Number(slider.value), state?.duration_ms ?? 0));
  });
  const stop = (): void => { if (volume) endVolumeEdit(); else slider.dataset.editing = 'false'; };
  slider.addEventListener('blur', stop);
  slider.addEventListener('pointercancel', stop);
}
/** Play the sheet's exit animation, then close; native close still restores focus. */
function closeSheet(): void {
  if (!sheet.open || sheet.classList.contains('closing')) return;
  sheet.classList.add('closing');
  afterExit(sheet, () => { if (sheet.classList.contains('closing')) sheet.close(); });
}
element('open-player').addEventListener('click', () => { if (!sheet.open) { sheet.classList.remove('closing'); sheet.showModal(); } });
element('close-player').addEventListener('click', closeSheet);
// Escape: animate instead of the instant native close (an uncancelable cancel still closes natively).
sheet.addEventListener('cancel', event => { event.preventDefault(); closeSheet(); });
sheet.addEventListener('close', () => { sheet.classList.remove('closing'); settleViewport(); });
// Native dialog handles focus trapping and restoration of focus.
let touchStart: number | undefined;
const sheetHeader = sheet.querySelector<HTMLElement>('.sheet-header')!;
sheetHeader.addEventListener('touchstart', event => { touchStart = event.touches[0]?.clientY; }, { passive: true });
sheetHeader.addEventListener('touchend', event => {
  if (touchStart !== undefined && (event.changedTouches[0]?.clientY ?? touchStart) - touchStart > 80) closeSheet();
  touchStart = undefined;
}, { passive: true });
document.addEventListener('visibilitychange', () => { document.body.classList.toggle('background', document.hidden); });
document.addEventListener('keydown', event => {
  const target = event.target;
  if (event.code !== 'Space' || event.repeat || !(target instanceof HTMLElement)
    || target.closest('input, textarea, select, button, a, [contenteditable]')) return;
  if (state?.track) { event.preventDefault(); void control(state.paused ? 'resume' : 'pause'); }
});
window.addEventListener('pagehide', () => { unlisten?.(); unlisten = undefined; demo?.dispose(); });

async function connect(): Promise<void> {
  if (!hasBackend) {
    setStatus('Предпросмотр интерфейса. Воспроизведение и поиск доступны в приложении Tauri.');
    element('demo-link').hidden = false;
    element('downloads-status').hidden = true;
    element('downloads-info').hidden = false;
    return;
  }
  element('demo-chip').hidden = !demo;
  try {
    if (!demo) unlisten = await listen<PlayerEvent>('player-event', ({ payload }) => handleEvent(payload));
    render(await command<Snapshot>('get_player_state'));
    if (!state?.error) {
      setStatus(demo
        ? 'Демо-режим: примеры треков и загрузок живут только в этой вкладке. Попробуй найти «Сати», «пусто» или «ошибка».'
        : 'Готово. История и профиль вкуса хранятся локально.');
    }
  } catch (error) { fail(error); }
  await refreshDownloads();
}
void connect();
