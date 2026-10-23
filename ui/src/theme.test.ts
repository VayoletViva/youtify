import { argbFromRgb, blueFromArgb, greenFromArgb, Hct, MaterialDynamicColors, redFromArgb, SchemeTonalSpot } from '@material/material-color-utilities';
import { contrast, dominantColor, palette, safeCoverUrl, type RGB } from './theme.js';

function assert(condition: boolean, message: string): void {
  if (!condition) throw new Error(message);
}

const roles = `background on-background primary on-primary primary-container on-primary-container
secondary on-secondary secondary-container on-secondary-container tertiary on-tertiary tertiary-container on-tertiary-container
surface surface-dim surface-bright surface-container-lowest surface-container-low surface-container surface-container-high
surface-container-highest on-surface surface-variant on-surface-variant outline outline-variant inverse-surface inverse-on-surface
inverse-primary error on-error error-container on-error-container shadow scrim surface-tint
primary-fixed primary-fixed-dim on-primary-fixed on-primary-fixed-variant
secondary-fixed secondary-fixed-dim on-secondary-fixed on-secondary-fixed-variant
tertiary-fixed tertiary-fixed-dim on-tertiary-fixed on-tertiary-fixed-variant`.split(/\s+/);

for (const rgb of [[255, 0, 0], [0, 255, 0], [0, 0, 255], [80, 120, 170], [0, 0, 0], [255, 255, 255], [128, 128, 128], [190, 222, 146], [253.5, 3.5, 2]] as RGB[]) {
  const colors = palette(rgb);
  const parse = (value: string): RGB => value.match(/\d+/g)!.map(Number) as unknown as RGB;
  assert(Object.keys(colors).length === roles.length, 'Complete role set');
  const scheme = new SchemeTonalSpot(Hct.fromInt(argbFromRgb(...rgb)), true, 0);
  const official = new MaterialDynamicColors();
  const officialRoles = [...official.allColors, official.surfaceVariant(), official.shadow(), official.scrim(), official.surfaceTint()];
  for (const role of roles) {
    const value = colors[`--md-sys-color-${role}`];
    assert(/^rgb\(\d+ \d+ \d+\)$/.test(value), `${role}: RGB output for ${rgb}`);
    assert(parse(value).every(v => Number.isFinite(v) && v >= 0 && v <= 255), `${role}: finite gamut`);
    const dynamic = officialRoles.find(color => color.name === role.replace(/-/g, '_'));
    assert(dynamic !== undefined, `${role}: official role`);
    const argb = dynamic!.getArgb(scheme);
    assert(value === `rgb(${redFromArgb(argb)} ${greenFromArgb(argb)} ${blueFromArgb(argb)})`, `${role}: official HCT output`);
  }
  for (const [background, foreground] of [
    ['primary', 'on-primary'], ['primary-container', 'on-primary-container'],
    ['secondary', 'on-secondary'], ['secondary-container', 'on-secondary-container'],
    ['tertiary', 'on-tertiary'], ['tertiary-container', 'on-tertiary-container'],
    ['error', 'on-error'], ['error-container', 'on-error-container'],
    ['inverse-surface', 'inverse-on-surface'], ['inverse-surface', 'inverse-primary'],
    ...['surface', 'surface-dim', 'surface-bright', 'surface-container-lowest', 'surface-container-low',
      'surface-container', 'surface-container-high', 'surface-container-highest'].flatMap(surface =>
      [[surface, 'on-surface'], [surface, 'on-surface-variant']]),
  ]) {
    assert(contrast(parse(colors[`--md-sys-color-${background}`]), parse(colors[`--md-sys-color-${foreground}`])) >= 4.5, `${background}/${foreground} contrast for ${rgb}`);
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
