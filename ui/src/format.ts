// Pure text helpers (no DOM) so the node test runner can cover them.

export const formatTime = (ms: number): string => {
  const seconds = Math.max(0, Math.floor(ms / 1000));
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, '0')}`;
};
/** Megabytes with a Russian decimal comma; whole numbers once the decimal stops being informative. */
export function formatSize(bytes: number): string {
  const mb = Math.max(0, bytes) / 1048576;
  return `${(mb >= 100 ? mb.toFixed(0) : mb.toFixed(1)).replace('.', ',')} МБ`;
}
/** Whole percent, or null when the server did not send a length (indeterminate progress). */
export function downloadPercent(done: number, total: number | null): number | null {
  if (!total || total <= 0) return null;
  return Math.min(100, Math.max(0, Math.floor((done / total) * 100)));
}
export function waitText(seconds: number): string {
  if (seconds < 60) return `${Math.max(1, Math.ceil(seconds))} с`;
  const minutes = Math.ceil(seconds / 60);
  return minutes >= 60 ? `${Math.floor(minutes / 60)} ч ${minutes % 60} мин` : `${minutes} мин`;
}

/** Short Russian explanations for known backend errors; the raw text stays visible in app-status. */
const knownErrors: [RegExp, string][] = [
  [/Already downloaded/, 'Этот трек уже скачан.'],
  [/Download already in progress/, 'Этот трек уже скачивается.'],
  [/online discovery found nothing/, 'Не нашли новых треков в сети. Проверь подключение или найди пару треков вручную — волна подстроится под них.'],
  [/HTTP 451.*; (SoundCloud|Deezer)/, 'Spotify недоступен в твоём регионе, а через Deezer и SoundCloud этот трек не нашёлся. Попробуй другое название.'],
  [/HTTP 451/, 'Spotify недоступен в твоём регионе. Поиск по названию сам переключается на Deezer, а плейлисты импортируй по ссылке Deezer или SoundCloud.'],
  [/HLS: (encrypted|master|live|byte-range)/, 'Этот поток SoundCloud защищён или устроен нестандартно. Ищем другой источник.'],
  [/native (HLS )?decode failed|Opus needs an external/, 'Встроенный декодер не справился с форматом этого трека. Попробуй другой трек или источник.'],
  [/HTTP 403/, 'Сервис отказал в доступе к потоку (403). Возможно, ссылка устарела — повтори воспроизведение.'],
  [/transport error|download failed|truncated range|timed out|timeout/i, 'Проблема с сетью при загрузке потока. Проверь подключение и повтори.'],
  [/no full-length|no match for/, 'Не нашли полную версию трека на YouTube или SoundCloud.'],
];
export function errorText(message: unknown): string {
  const text = String(message);
  // Checked before the table: "Download failed" would otherwise hit the generic network row.
  const failed = text.match(/Download failed: (.*)/s);
  if (failed) return `Не удалось скачать трек: ${failed[1].trim() || 'неизвестная ошибка'}`;
  const geo = text.match(/Spotify playlist unavailable in this country: (.*)/s);
  if (geo) {
    const name = geo[1].trim();
    return `В твоём регионе Spotify не отдаёт список треков плейлиста${name ? ` «${name}»` : ''}. Найди этот плейлист в Deezer или SoundCloud и вставь ссылку на него.`;
  }
  const spotify = text.match(/Spotify: HTTP 429 \(rate limited\); retry in (\d+) seconds/);
  if (spotify) return `Spotify временно ограничил этот запрос (ещё ${waitText(Number(spotify[1]))}). Поиск и ссылки на треки работают отдельно — попробуй их.`;
  const deezer = text.match(/Deezer: HTTP 429 \(rate limited\); retry in (\d+) seconds/);
  if (deezer) return `Deezer временно ограничил запросы (ещё ${waitText(Number(deezer[1]))}). Подожди немного или выбери SoundCloud.`;
  return knownErrors.find(([pattern]) => pattern.test(text))?.[1] ?? text;
}
