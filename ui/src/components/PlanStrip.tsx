import { ChevronDown, ScrollText } from "lucide-react";
import { useEffect, useRef, useState } from "react";

/** Docked strip above the composer while a plan awaits the user's decision.
 * It owns the plan actions (the inline card renders compact, buttonless) so
 * the approval controls never scroll away with the transcript. Disappears when
 * the prompt resolves (the server re-emits the message with `resolved`).
 *
 * Claude-desktop parity — the actions mean:
 *  - Reject: plain rejection, no feedback; the model stops and waits.
 *  - Revise…: focuses the composer; typed text is sent as revision feedback
 *    (the composer routes it to this card — see ChatPanel's send()).
 *  - Accept and auto mode (primary): approve + resume under Auto — the
 *    default accept action. The caret menu holds the two guarded tiers:
 *    accept-edits (edits allowed, everything else still gated) and
 *    bypass-everything.
 *  - Open plan: link in the title row → the right-pane plan tab. */
export function PlanStrip({
  synthesized,
  onView,
  onApprove,
  onReject,
  onRevise,
}: {
  /** Card synthesized from the turn's final text (no ExitPlanMode call). */
  synthesized: boolean;
  onView: () => void;
  onApprove: (resumeMode: "auto" | "accept-edits" | "bypass") => void;
  onReject: () => void;
  onRevise: () => void;
}) {
  const [menuOpen, setMenuOpen] = useState(false);
  const menuRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    if (!menuOpen) return;
    const close = (e: PointerEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) setMenuOpen(false);
    };
    window.addEventListener("pointerdown", close);
    return () => window.removeEventListener("pointerdown", close);
  }, [menuOpen]);

  return (
    <div className="plan-strip">
      <div className="plan-strip-info">
        <ScrollText size={14} className="plan-strip-icon" />
        <span className="plan-strip-title">
          {synthesized ? "Claude is ready to proceed" : "Claude proposed a plan"}
        </span>
        <button className="plan-strip-open" onClick={onView}>
          Open plan
        </button>
      </div>
      <div className="prompt-actions plan-strip-actions">
        <button className="btn-ghost" onClick={onReject}>
          Reject
        </button>
        <button className="btn-ghost" onClick={onRevise}>
          Revise…
        </button>
        <span className="plan-strip-spacer" />
        <div className="plan-strip-approve" ref={menuRef}>
          <button className="btn-primary plan-strip-primary" onClick={() => onApprove("auto")}>
            Accept and auto mode
          </button>
          <button
            className="btn-primary plan-strip-primary plan-strip-caret"
            aria-label="More approval options"
            onClick={() => setMenuOpen((o) => !o)}
          >
            <ChevronDown size={13} />
          </button>
          {menuOpen && (
            <div className="plan-strip-menu">
              <button
                onClick={() => {
                  setMenuOpen(false);
                  onApprove("accept-edits");
                }}
              >
                Accept
              </button>
              <button
                onClick={() => {
                  setMenuOpen(false);
                  onApprove("bypass");
                }}
              >
                Accept and bypass all
              </button>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
