import { downloadPercent, errorText, formatSize, formatTime, waitText } from './format.js';

function assert(condition: boolean, message: string): void {
  if (!condition) throw new Error(message);
}

assert(formatTime(0) === '0:00' && formatTime(61_999) === '1:01' && formatTime(-5) === '0:00', 'Track time');
assert(formatSize(0) === '0,0 МБ' && formatSize(3_670_016) === '3,5 МБ' && formatSize(150 * 1048576) === '150 МБ', 'Size in MB');
assert(downloadPercent(5, null) === null && downloadPercent(5, 0) === null, 'Unknown length is indeterminate');
assert(downloadPercent(50, 200) === 25 && downloadPercent(300, 200) === 100 && downloadPercent(199, 200) === 99, 'Percent clamps and never shows 100 early');
assert(waitText(5) === '5 с' && waitText(61) === '2 мин' && waitText(3_700) === '1 ч 2 мин', 'Wait text');

const geo = errorText('Spotify playlist unavailable in this country: Chill Mix');
assert(geo.includes('«Chill Mix»') && geo.includes('Deezer') && geo.includes('SoundCloud'), 'Geo-blocked playlist names it and suggests alternatives');
assert(errorText('Deezer: HTTP 429 (rate limited); retry in 5 seconds').startsWith('Deezer временно ограничил запросы (ещё 5 с)'), 'Deezer rate limit');
assert(errorText('Spotify: HTTP 429 (rate limited); retry in 7200 seconds').includes('ещё 2 ч 0 мин'), 'Spotify rate limit');
assert(errorText('Already downloaded') === 'Этот трек уже скачан.', 'Already downloaded');
assert(errorText('Download already in progress') === 'Этот трек уже скачивается.', 'Download in progress');
// Must not fall into the generic "download failed" network row; the reason stays visible.
assert(errorText('Download failed: disk full') === 'Не удалось скачать трек: disk full', 'Download failure keeps detail');
assert(errorText('stream download failed').startsWith('Проблема с сетью'), 'Stream network error unchanged');
assert(errorText('wave: online discovery found nothing').startsWith('Не нашли новых треков'), 'Discovery found nothing');
assert(errorText('Spotify: HTTP 451; SoundCloud: no match for x').includes('Deezer и SoundCloud'), 'Fallback miss');
assert(errorText('Spotify: HTTP 451').includes('Deezer'), 'Region block mentions automatic Deezer fallback');
assert(errorText('something new') === 'something new', 'Unknown errors pass through');
