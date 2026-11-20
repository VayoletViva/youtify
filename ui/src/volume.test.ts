import { gainToSlider, sliderToGain, volumeLabel } from './volume.js';

function assert(condition: boolean, message: string): void {
  if (!condition) throw new Error(message);
}

assert(sliderToGain(0) === 0 && sliderToGain(1) === 1, 'Curve endpoints');
assert(sliderToGain(0.5) === 0.125, 'Middle of the slider is gain 0.125 (about -18 dB), not 0.5');
let previous = -1;
for (let step = 0; step <= 20; step++) {
  const gain = sliderToGain(step * 0.05);
  assert(gain > previous, `Monotonic at ${step * 0.05}`);
  previous = gain;
}
assert(sliderToGain(-1) === 0 && sliderToGain(2) === 1, 'Positions clamp to [0, 1]');
assert(sliderToGain(Number.NaN) === 0 && sliderToGain(Number.POSITIVE_INFINITY) === 0, 'Non-finite position is silence');
assert(gainToSlider(Number.NaN) === 1, 'Unknown gain keeps the slider at full');
assert(gainToSlider(-0.5) === 0 && gainToSlider(5) === 1, 'Gain clamps to [0, 1]');
for (let step = 0; step <= 100; step++) {
  const x = step / 100;
  assert(Math.abs(gainToSlider(sliderToGain(x)) - x) < 1e-9, `Round trip at ${x}`);
}
assert(gainToSlider(0.125) > 0.49 && gainToSlider(0.125) < 0.51, 'Gain 0.125 shows the slider at the middle');
assert(volumeLabel(0.5) === '50\u00a0%' && volumeLabel(1) === '100\u00a0%' && volumeLabel(Number.NaN) === '0\u00a0%', 'Volume label with a non-breaking space');
console.log('Volume assertions passed');
