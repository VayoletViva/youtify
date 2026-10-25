import { argbFromRgb, blueFromArgb, greenFromArgb, Hct, MaterialDynamicColors, redFromArgb, SchemeTonalSpot } from '@material/material-color-utilities';

export type RGB = readonly [number, number, number];
const FALLBACK: RGB = [190, 222, 146];
const linear = (v: number): number => (v /= 255) <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4;

/** 5-bit/channel quantization, retaining the mean RGB of the largest bin. */
export function dominantColor(pixels: Uint8ClampedArray): RGB {
  const bins = new Map<number, { count: number; r: number; g: number; b: number }>();
  for (let i = 0; i + 3 < pixels.length; i += 4) {
    const r = pixels[i], g = pixels[i + 1], b = pixels[i + 2];
    if (pixels[i + 3] < 128) continue;
    const key = ((r >> 3) << 10) | ((g >> 3) << 5) | (b >> 3);
    const bin = bins.get(key) ?? { count: 0, r: 0, g: 0, b: 0 };
    bin.count++; bin.r += r; bin.g += g; bin.b += b;
    bins.set(key, bin);
  }
  let best: { count: number; r: number; g: number; b: number } | undefined;
  for (const bin of bins.values()) if (!best || bin.count > best.count) best = bin;
  return best ? [best.r / best.count, best.g / best.count, best.b / best.count] : FALLBACK;
}

export function contrast(a: RGB, b: RGB): number {
  const luminance = (rgb: RGB): number => {
    const [r, g, b] = rgb.map(linear);
    return 0.2126 * r + 0.7152 * g + 0.0722 * b;
  };
  const x = luminance(a), y = luminance(b);
  return (Math.max(x, y) + 0.05) / (Math.min(x, y) + 0.05);
}

export function palette(seed: RGB, dark = true): Record<string, string> {
  const scheme = new SchemeTonalSpot(Hct.fromInt(argbFromRgb(...seed)), dark, 0);
  const colors = new MaterialDynamicColors();
  const roles = [
    'background', 'onBackground', 'primary', 'onPrimary', 'primaryContainer', 'onPrimaryContainer',
    'secondary', 'onSecondary', 'secondaryContainer', 'onSecondaryContainer',
    'tertiary', 'onTertiary', 'tertiaryContainer', 'onTertiaryContainer',
    'surface', 'surfaceDim', 'surfaceBright', 'surfaceContainerLowest', 'surfaceContainerLow',
    'surfaceContainer', 'surfaceContainerHigh', 'surfaceContainerHighest', 'onSurface',
    'surfaceVariant', 'onSurfaceVariant', 'outline', 'outlineVariant',
    'inverseSurface', 'inverseOnSurface', 'inversePrimary',
    'error', 'onError', 'errorContainer', 'onErrorContainer', 'shadow', 'scrim', 'surfaceTint',
    'primaryFixed', 'primaryFixedDim', 'onPrimaryFixed', 'onPrimaryFixedVariant',
    'secondaryFixed', 'secondaryFixedDim', 'onSecondaryFixed', 'onSecondaryFixedVariant',
    'tertiaryFixed', 'tertiaryFixedDim', 'onTertiaryFixed', 'onTertiaryFixedVariant',
  ] as const;
  return Object.fromEntries(roles.map(role => {
    const argb = colors[role]().getArgb(scheme);
    const name = role.replace(/[A-Z]/g, letter => `-${letter.toLowerCase()}`);
    return [`--md-sys-color-${name}`, `rgb(${redFromArgb(argb)} ${greenFromArgb(argb)} ${blueFromArgb(argb)})`];
  }));
}

export function safeCoverUrl(raw: string | null): string | null {
  if (!raw || raw.length > 4096) return null;
  try {
    const url = new URL(raw);
    return url.protocol === 'https:' && !url.username && !url.password ? url.href : null;
  } catch { return null; }
}

/** A single 64x64 canvas, one image request, 24 cached seeds. CORS failures are normal. */
export class DynamicTheme {
  private readonly canvas = document.createElement('canvas');
  private readonly cache = new Map<string, RGB>();
  private generation = 0;
  private cancel: (() => void) | undefined;
  private seed: RGB = FALLBACK;
  private readonly scheme = window.matchMedia('(prefers-color-scheme: dark)');

  constructor() {
    this.canvas.width = this.canvas.height = 64;
    this.canvas.hidden = true;
    this.apply(FALLBACK);
    requestAnimationFrame(() => requestAnimationFrame(() => document.documentElement.classList.add('theme-ready')));
    // Follow the system light/dark setting, as Android's Material You does.
    this.scheme.addEventListener('change', () => this.apply(this.seed));
  }

  private apply(seed: RGB): void {
    this.seed = seed;
    const dark = this.scheme.matches;
    const root = document.documentElement;
    const colors = palette(seed, dark);
    for (const [name, value] of Object.entries(colors)) root.style.setProperty(name, value);
    root.style.colorScheme = dark ? 'dark' : 'light';
    document.querySelector('meta[name="theme-color"]')?.setAttribute('content', colors['--md-sys-color-surface']);
  }

  async update(raw: string | null): Promise<void> {
    const generation = ++this.generation;
    this.cancel?.();
    const url = safeCoverUrl(raw);
    if (!url) { this.apply(FALLBACK); return; }
    const cached = this.cache.get(url);
    if (cached) { this.apply(cached); return; }
    const image = new Image();
    image.crossOrigin = 'anonymous';
    image.referrerPolicy = 'no-referrer';
    try {
      await new Promise<void>((resolve, reject) => {
        const finish = (error?: Error): void => {
          clearTimeout(timer); image.onload = image.onerror = null;
          if (generation === this.generation) this.cancel = undefined;
          if (error) { image.src = ''; reject(error); } else resolve();
        };
        const timer = setTimeout(() => finish(new Error('Cover timeout')), 8000);
        this.cancel = () => finish(new Error('Superseded'));
        image.onload = () => finish();
        image.onerror = () => finish(new Error('Cover unavailable or CORS denied'));
        image.src = url;
      });
      if (generation !== this.generation) return;
      // Avoid decoding unusually large artwork into additional canvas surfaces.
      if (image.naturalWidth * image.naturalHeight > 25_000_000) throw new Error('Cover too large');
      const ctx = this.canvas.getContext('2d', { willReadFrequently: true });
      if (!ctx) throw new Error('Canvas unavailable');
      ctx.clearRect(0, 0, 64, 64);
      ctx.drawImage(image, 0, 0, 64, 64);
      const seed = dominantColor(ctx.getImageData(0, 0, 64, 64).data);
      if (this.cache.size >= 24) this.cache.delete(this.cache.keys().next().value!);
      this.cache.set(url, seed);
      this.apply(seed);
    } catch {
      if (generation === this.generation) this.apply(FALLBACK);
    }
  }
}
