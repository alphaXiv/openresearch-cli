import { ChevronDown, ScrollText } from "lucide-react";
import { useEffect, useRef, useState } from "react";

/** Docked strip above the composer while a plan awaits the user's decision.
 * It owns the plan actions (the inline card renders compact, buttonless) so
 * the approval controls never scroll away with the transcript. Disappears when
 * the prompt resolves (the server re-emits the message with `resolved`). */
export function PlanStrip({
  plan,
  synthesized,
  onView,
  onApprove,
  onKeepPlanning,
}: {
  plan: string;
  /** Card synthesized from the turn's final text (no ExitPlanMode call). */
  synthesized: boolean;
  onView: () => void;
  onApprove: (resumeMode: "auto" | "accept-edits" | "bypass") => void;
  onKeepPlanning: () => void;
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

  // One-line excerpt: the first heading or non-empty line.
  const excerpt =
    plan
      .split("\n")
      .map((l) => l.replace(/^#+\s*/, "").trim())
      .find((l) => l.length > 0) ?? "";

  return (
    <div className="plan-strip">
      <div className="plan-strip-info">
        <ScrollText size={14} className="plan-strip-icon" />
        <div className="plan-strip-text">
          <span className="plan-strip-title">
            {synthesized ? "Plan mode — ready to proceed?" : "Plan ready"}
          </span>
          {excerpt && <span className="plan-strip-excerpt">{excerpt}</span>}
        </div>
      </div>
      <div className="plan-strip-actions">
        <button className="btn-ghost" onClick={onView}>
          View plan
        </button>
        <button className="btn-ghost" onClick={onKeepPlanning}>
          Keep planning
        </button>
        <div className="plan-strip-approve" ref={menuRef}>
          <button className="btn-primary" onClick={() => onApprove("auto")}>
            Approve &amp; run
          </button>
          <button
            className="btn-primary plan-strip-caret"
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
                Approve &amp; accept edits
              </button>
              <button
                onClick={() => {
                  setMenuOpen(false);
                  onApprove("bypass");
                }}
              >
                Approve &amp; bypass all
              </button>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
