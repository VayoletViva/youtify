// Perceptual volume curve (no DOM). The slider stores a position in [0, 1]; the backend gets linear gain.
// Loudness is roughly logarithmic, so a linear slider puts almost all of the audible change in its first
// third. A cubic curve (gain = p³, about -18 dB at the middle) is the usual approximation.

const unit = (value: number): number => Math.min(1, Math.max(0, value));

/** Slider position -> linear gain for control_playback('volume'). NaN or infinite input is silence. */
export function sliderToGain(position: number): number {
  if (!Number.isFinite(position)) return 0;
  return unit(position) ** 3;
}

/** Linear gain from a snapshot -> slider position. NaN keeps the slider at full (the engine default). */
export function gainToSlider(gain: number): number {
  if (Number.isNaN(gain)) return 1;
  return Math.cbrt(unit(gain));
}

/** Text for aria-valuetext: the slider position in whole percent, with a non-breaking space. */
export function volumeLabel(position: number): string {
  const value = Number.isFinite(position) ? unit(position) : 0;
  return `${Math.round(value * 100)}\u00a0%`;
}
