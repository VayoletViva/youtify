import { contrast, dominantColor, oklchToRgb, palette, rgbToOklch, safeCoverUrl, type RGB } from './theme.js';

function assert(condition: boolean, message: string): void {
  if (!condition) throw new Error(message);
}

for (const rgb of [[255, 0, 0], [0, 255, 0], [0, 0, 255], [80, 120, 170], [0, 0, 0], [255, 255, 255]] as RGB[]) {
  const back = oklchToRgb(rgbToOklch(rgb));
  assert(back.every((value, i) => Math.abs(value - rgb[i]) <= 1), `OKLCH round trip: ${rgb}`);
  const colors = palette(rgb);
  const parse = (value: string): RGB => value.match(/\d+/g)!.map(Number) as unknown as RGB;
  assert(contrast(parse(colors['--md-sys-color-primary']), parse(colors['--md-sys-color-on-primary'])) >= 4.5, 'Primary contrast');
  assert(contrast(parse(colors['--md-sys-color-surface-container']), parse(colors['--md-sys-color-on-surface'])) >= 4.5, 'Surface contrast');
  for (const [background, foreground] of [
    ['primary-container', 'on-primary-container'], ['tertiary', 'on-tertiary'],
    ['surface-container-low', 'on-surface-variant'], ['secondary-container', 'on-secondary-container'],
  ]) {
    assert(contrast(parse(colors[`--md-sys-color-${background}`]), parse(colors[`--md-sys-color-${foreground}`])) >= 4.5, `${background} contrast`);
  }
}
const dominant = dominantColor(new Uint8ClampedArray([255, 0, 0, 255, 255, 0, 0, 255, 0, 0, 255, 255, 0, 255, 0, 0]));
assert(dominant[0] === 255 && dominant[1] === 0 && dominant[2] === 0, 'Dominant quantized bin');
assert(dominantColor(new Uint8ClampedArray()).every(Number.isFinite), 'Empty image fallback');
assert(safeCoverUrl('javascript:alert(1)') === null, 'Reject scripts');
assert(safeCoverUrl('http://example.com/a.jpg') === null, 'Reject insecure covers');
assert(safeCoverUrl('https://user:secret@example.com/a.jpg') === null, 'Reject credentials');
assert(safeCoverUrl('https://example.com/a.jpg') !== null, 'Accept HTTPS covers');
console.log('Theme assertions passed');
