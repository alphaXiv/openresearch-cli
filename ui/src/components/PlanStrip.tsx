import { ChevronDown, CornerDownLeft, ScrollText } from "lucide-react";
import { useEffect, useRef, useState } from "react";

/** Docked strip above the composer while a plan awaits the user's decision.
 * It owns the plan actions (the inline card renders compact, buttonless) so
 * the approval controls never scroll away with the transcript. Disappears when
 * the prompt resolves (the server re-emits the message with `resolved`).
 *
 * Claude-desktop parity — the actions mean:
 *  - Reject: plain rejection, no feedback; the model stops and waits.
 *  - Revise…: swaps the strip into its own inline textarea ("What should
 *    change? (optional)") with Back/Revise buttons — self-contained, not a
 *    detour through the main composer.
 *  - Accept and auto mode (primary): approve + resume under Auto — the
 *    default accept action. The caret menu holds Accept and bypass all
 *    (skip every gate, not just Auto's). No plain "accept-edits" tier here —
 *    the app has no story for partial (edits-only) approval.
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
  onApprove: (resumeMode: "auto" | "bypass") => void;
  onReject: () => void;
  /** Revision feedback; always non-empty (a blank submit sends a generic
   * "please revise" — note presence is what distinguishes revise from
   * reject on the wire). */
  onRevise: (note: string) => void;
}) {
  const [menuOpen, setMenuOpen] = useState(false);
  const menuRef = useRef<HTMLDivElement | null>(null);
  const [revising, setRevising] = useState(false);
  const [note, setNote] = useState("");
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    if (!menuOpen) return;
    const close = (e: PointerEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) setMenuOpen(false);
    };
    window.addEventListener("pointerdown", close);
    return () => window.removeEventListener("pointerdown", close);
  }, [menuOpen]);

  useEffect(() => {
    if (revising) textareaRef.current?.focus();
  }, [revising]);

  const submitRevision = () => {
    // A blank submit still means "revise": the wire distinguishes reject
    // (no note) from revise (note), so an empty field gets a generic nudge
    // rather than accidentally reading as a hard rejection. (The backend
    // wraps it as "Keep refining the plan: <note>", so word it to read well
    // there.)
    onRevise(note.trim() || "no specific feedback — use your judgment");
    setNote("");
    setRevising(false);
  };

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
      {revising ? (
        <>
          <textarea
            ref={textareaRef}
            className="plan-strip-revise-input"
            placeholder="What should change? (optional)"
            rows={2}
            value={note}
            onChange={(e) => setNote(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Escape") {
                e.preventDefault();
                setNote("");
                setRevising(false);
              } else if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                submitRevision();
              }
            }}
          />
          <div className="prompt-actions plan-strip-actions">
            <button
              className="btn-ghost"
              onClick={() => {
                setNote("");
                setRevising(false);
              }}
            >
              Back
            </button>
            <span className="plan-strip-spacer" />
            <button className="btn-primary plan-strip-primary" onClick={submitRevision}>
              Revise
              <CornerDownLeft size={13} />
            </button>
          </div>
        </>
      ) : (
        <div className="prompt-actions plan-strip-actions">
          <button className="btn-ghost" onClick={onReject}>
            Reject
          </button>
          <button className="btn-ghost" onClick={() => setRevising(true)}>
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
                    onApprove("bypass");
                  }}
                >
                  Accept and bypass all
                </button>
              </div>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
