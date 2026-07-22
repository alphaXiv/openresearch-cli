import { fmtTokens, type ContextUsage } from "../api";
import { usePopover } from "./ModelPicker";
import { ProgressBar } from "./ProgressBar";

/** Amber ≥80%, red ≥95% — mirrors Claude Desktop's context meter. */
function tone(pct: number): string {
  if (pct >= 95) return "var(--accent-red)";
  if (pct >= 80) return "var(--accent-amber)";
  return "var(--accent)";
}

const RING_R = 6.5;
const RING_C = 2 * Math.PI * RING_R;

/** Composer meter: how much of the model's context window this session has
 * used, drawn as a small progress ring (token-count text when the window is
 * unknown). Hidden until the harness first reports usage; the popover holds
 * the breakdown. */
export function ContextMeter({ usage }: { usage?: ContextUsage }) {
  const { open, setOpen, ref } = usePopover();
  if (!usage) return null;

  const { usedTokens, contextWindow } = usage;
  const pct =
    contextWindow && contextWindow > 0
      ? Math.min(100, Math.round((usedTokens / contextWindow) * 100))
      : null;
  const fill = pct === null ? "var(--accent)" : tone(pct);

  return (
    <div className="option-picker" ref={ref}>
      <button
        type="button"
        className="composer-bare context-ring"
        title="Context window used"
        onClick={() => setOpen((v) => !v)}
      >
        {pct === null ? (
          fmtTokens(usedTokens)
        ) : (
          <svg viewBox="0 0 16 16" width="16" height="16" aria-hidden="true">
            <circle
              cx="8"
              cy="8"
              r={RING_R}
              fill="none"
              stroke="var(--border)"
              strokeWidth="2.5"
            />
            <circle
              cx="8"
              cy="8"
              r={RING_R}
              fill="none"
              stroke={fill}
              strokeWidth="2.5"
              strokeLinecap="round"
              strokeDasharray={`${(RING_C * Math.max(pct, 2)) / 100} ${RING_C}`}
              transform="rotate(-90 8 8)"
            />
          </svg>
        )}
      </button>
      {open && (
        <div className="option-menu align-right context-meter-menu">
          <div className="context-meter-head">
            <span>Context window</span>
            <span className="context-meter-value">
              {pct === null
                ? `${fmtTokens(usedTokens)} tokens`
                : `${fmtTokens(usedTokens)} / ${fmtTokens(contextWindow!)} (${pct}%)`}
            </span>
          </div>
          {pct !== null && (
            <ProgressBar value={usedTokens} max={contextWindow!} fillColor={fill} />
          )}
        </div>
      )}
    </div>
  );
}
