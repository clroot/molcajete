// The remote display scale: kept as a DPI (the `dpi` connection parameter,
// 96 per 100 %) and shown in percent.

export const AUTOMATIC_SCALE_LABEL = 'Automatic (follow this screen)';

const PERCENTS = [100, 125, 150, 175, 200, 250, 300];

export const dpiOfPercent = (percent: number) => Math.round((96 * percent) / 100);
export const percentOfDpi = (dpi: number) => Math.round((dpi * 100) / 96);

/** The offered scales, with a DPI string as the value. */
export const SCALE_OPTIONS = PERCENTS.map(percent => ({
  value: String(dpiOfPercent(percent)),
  label: `${percent} %`,
}));

/** The options, plus `dpi` when it is none of them (typed in by hand). */
export function scaleOptionsWith(dpi: string) {
  if (!/^\d+$/.test(dpi) || SCALE_OPTIONS.some(o => o.value === dpi)) return SCALE_OPTIONS;
  return [...SCALE_OPTIONS, {value: dpi, label: `${percentOfDpi(Number(dpi))} %`}];
}

/** The DPI that "Automatic" stands for on this screen. */
export const screenDpi = () => Math.round(96 * (window.devicePixelRatio || 1));
