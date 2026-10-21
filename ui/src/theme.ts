/** Dependency-free Material You-inspired OKLCH palette, not an HCT approximation. */
export type RGB = readonly [number, number, number];
export type LCH = readonly [number, number, number];
const FALLBACK: RGB = [190, 222, 146];
const linear = (v: number): number => (v /= 255) <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4;
const gamma = (v: number): number => 255 * (v <= 0.0031308 ? 12.92 * v : 1.055 * v ** (1 / 2.4) - 0.055);

export function rgbToOklch(rgb: RGB): LCH {
  const [r, g, b] = rgb.map(linear);
  const l = Math.cbrt(0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b);
  const m = Math.cbrt(0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b);
  const s = Math.cbrt(0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b);
  const light = 0.2104542553 * l + 0.793617785 * m - 0.0040720468 * s;
  const a = 1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s;
  const bb = 0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s;
  return [light, Math.hypot(a, bb), (Math.atan2(bb, a) * 180 / Math.PI + 360) % 360];
}

function toLinear([light, chroma, hue]: LCH): RGB {
  const a = chroma * Math.cos(hue * Math.PI / 180);
  const b = chroma * Math.sin(hue * Math.PI / 180);
  const l = (light + 0.3963377774 * a + 0.2158037573 * b) ** 3;
  const m = (light - 0.1055613458 * a - 0.0638541728 * b) ** 3;
  const s = (light - 0.0894841775 * a - 1.291485548 * b) ** 3;
  return [4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
    -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
    -0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s];
}

/** Reduce chroma in OKLCH, preserving hue/lightness instead of clipping channels. */
export function oklchToRgb(lch: LCH): RGB {
  const [light, chroma, hue] = lch;
  let lo = 0, hi = chroma;
  let rgb = toLinear(lch);
  if (rgb.some(v => v < 0 || v > 1)) {
    for (let i = 0; i < 18; i++) {
      const mid = (lo + hi) / 2;
      if (toLinear([light, mid, hue]).every(v => v >= 0 && v <= 1)) lo = mid;
      else hi = mid;
    }
    rgb = toLinear([light, lo, hue]);
  }
  return rgb.map(v => Math.round(Math.max(0, Math.min(255, gamma(v))))) as unknown as RGB;
}

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

export function palette(seed: RGB): Record<string, string> {
  const [, c, h] = rgbToOklch(seed);
  const hue = c < 0.015 ? 130 : h;
  const primary = oklchToRgb([0.82, Math.min(0.16, Math.max(0.08, c)), hue]);
  const onPrimary: RGB = contrast(primary, [0, 0, 0]) >= contrast(primary, [255, 255, 255]) ? [0, 0, 0] : [255, 255, 255];
  const colors: Record<string, RGB> = {
    primary, 'on-primary': onPrimary,
    'primary-container': oklchToRgb([0.33, 0.065, hue]),
    'on-primary-container': oklchToRgb([0.93, 0.065, hue]),
    tertiary: oklchToRgb([0.82, 0.075, hue + 65]),
    'on-tertiary': oklchToRgb([0.23, 0.035, hue + 65]),
    surface: oklchToRgb([0.16, 0.012, hue]),
    'surface-container-low': oklchToRgb([0.19, 0.018, hue]),
    'surface-container': oklchToRgb([0.22, 0.022, hue]),
    'surface-container-high': oklchToRgb([0.28, 0.03, hue]),
    'secondary-container': oklchToRgb([0.34, 0.06, hue]),
    'on-secondary-container': oklchToRgb([0.94, 0.025, hue]),
    'on-surface': oklchToRgb([0.94, 0.01, hue]),
    'on-surface-variant': oklchToRgb([0.79, 0.025, hue]),
    'outline-variant': oklchToRgb([0.39, 0.025, hue]),
  };
  return Object.fromEntries(Object.entries(colors).map(([key, rgb]) => [`--md-sys-color-${key}`, `rgb(${rgb.join(' ')})`]));
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

  constructor() {
    this.canvas.width = this.canvas.height = 64;
    this.canvas.hidden = true;
    this.apply(FALLBACK);
  }

  private apply(seed: RGB): void {
    for (const [name, value] of Object.entries(palette(seed))) document.documentElement.style.setProperty(name, value);
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
