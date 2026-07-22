import type { ReactNode } from "react";

/** Determinate progress bar with a percent + optional caption. Shared by the
 * data-dir move card and the chat context-window meter. `fillColor` overrides
 * the default accent (the meter tints it amber/red as it fills). */
export function ProgressBar({
  value,
  max,
  label,
  caption,
  fillColor,
}: {
  value: number;
  max: number;
  /** Left caption; defaults to the computed percent. */
  label?: string;
  /** Right caption (e.g. a byte or token detail). */
  caption?: ReactNode;
  fillColor?: string;
}) {
  const pct = max > 0 ? Math.min(100, Math.round((value / max) * 100)) : 0;
  return (
    <div className="progress" role="progressbar" aria-valuenow={pct} aria-valuemin={0} aria-valuemax={100}>
      <div className="progress-track">
        <div className="progress-fill" style={{ width: `${pct}%`, background: fillColor }} />
      </div>
      <div className="progress-caption">
        <span>{label ?? `${pct}%`}</span>
        {caption}
      </div>
    </div>
  );
}
