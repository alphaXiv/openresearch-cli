import { fmtTokens, type ContextUsage } from "../api";
import { usePopover } from "./ModelPicker";
import { ProgressBar } from "./ProgressBar";

/** Amber ≥80%, red ≥95% — mirrors Claude Desktop's context meter. */
function tone(pct: number): string | undefined {
  if (pct >= 95) return "var(--accent-red)";
  if (pct >= 80) return "var(--accent-amber)";
  return undefined;
}

/** Composer meter: how much of the model's context window this session has used.
 * Hidden until the harness first reports usage. Shows the percent when the
 * window is known, else the raw token count; the popover breaks it down. */
export function ContextMeter({ usage }: { usage?: ContextUsage }) {
  const { open, setOpen, ref } = usePopover();
  if (!usage) return null;

  const { usedTokens, contextWindow } = usage;
  const pct =
    contextWindow && contextWindow > 0
      ? Math.min(100, Math.round((usedTokens / contextWindow) * 100))
      : null;
  const fill = pct === null ? undefined : tone(pct);

  return (
    <div className="option-picker" ref={ref}>
      <button
        type="button"
        className="composer-bare"
        title="Context window used"
        style={fill ? { color: fill } : undefined}
        onClick={() => setOpen((v) => !v)}
      >
        {pct === null ? fmtTokens(usedTokens) : `${pct}%`}
      </button>
      {open && (
        <div className="option-menu align-right context-meter-menu">
          <div className="model-group">Context window</div>
          {contextWindow && contextWindow > 0 ? (
            <>
              <ProgressBar value={usedTokens} max={contextWindow} fillColor={fill} />
              <div className="context-meter-caption">
                {fmtTokens(usedTokens)} of {fmtTokens(contextWindow)} tokens · {pct}%
              </div>
            </>
          ) : (
            <div className="context-meter-caption">
              {fmtTokens(usedTokens)} tokens · window unknown
            </div>
          )}
        </div>
      )}
    </div>
  );
}
