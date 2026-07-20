import { useEffect, useState } from "react";
import { createPortal } from "react-dom";
import { X } from "lucide-react";
import {
  type Bounds,
  type TourAnchor,
  useMeasure,
  usePopoverPosition,
  useTourBounds,
} from "./tourGeometry";

/** Set once the tour has been finished or skipped; gates the auto-start. */
export const TOUR_DONE_KEY = "orx:tour-done";

/** Breathing room between a target's edges and the spotlight cutout. */
const BOX_PADDING = 8;
/** Gap between the spotlight and the tour card. */
const CARD_DISTANCE = 20;

interface TourStep {
  /** data-onboarding ids to spotlight; null = centered card over a full dim. */
  focus: string[] | null;
  /** Which side of the target the card sits on; null = centered. */
  anchor: TourAnchor | null;
  title: string;
  description: string;
}

const STEPS: TourStep[] = [
  {
    focus: null,
    anchor: null,
    title: "Welcome to OpenResearch",
    description:
      "OpenResearch is your home for autoresearch. Spawn and coordinate research agents " +
      "in one workspace.",
  },
  {
    focus: ["composer"],
    anchor: "above",
    title: "Talk to your research agent",
    description:
      "Prompt your research agents to replicate a paper, create a baseline experiment, " +
      "run an eval, or investigate any research question. Type / for skills like " +
      "/reproduce-paper.",
  },
  {
    focus: ["model-picker"],
    anchor: "above",
    title: "Pick your model",
    description:
      "Choose a model from any harness you've connected: Claude Code, Codex, or OpenCode. " +
      "New sessions start with whatever you pick here, and each session keeps its harness.",
  },
  {
    focus: ["nav-files"],
    anchor: "right",
    title: "Reports and outputs",
    description:
      "The agent writes its reports, figures, and other outputs here, and anything you drop " +
      "in is visible to it too. Check Files after a run to see what came back.",
  },
  {
    focus: ["nav-compute"],
    anchor: "right",
    title: "Configure compute",
    description:
      "This is where compute is configured. Point runs at this machine, Modal, SSH boxes, " +
      "Kubernetes, or Slurm. Set it up once and agents pick the right hardware per run.",
  },
  {
    focus: ["experiments"],
    anchor: "left",
    title: "Follow every experiment",
    description:
      "Runs land here as a tree of experiments. Branch variants off a baseline, compare " +
      "results, and open any run's terminal or code changes in a tab.",
  },
  {
    focus: ["new-session"],
    anchor: "right",
    title: "Start a session",
    description:
      "Each session is its own agent working in its own worktree, so you can run several " +
      "agents in parallel. Ask for your first experiment whenever you're ready.",
  },
];

/**
 * The onboarding tour: a dimming overlay with a spotlight cut around the
 * focused element, plus an anchored card describing it. CSS transitions morph
 * the spotlight between steps. Targets are located by `data-onboarding`
 * attributes; a missing target degrades to a full dim with a centered card.
 */
export function Tour({ onClose }: { onClose: () => void }) {
  const [index, setIndex] = useState(0);
  const step = STEPS[index];
  const bounds = useTourBounds(step.focus ?? []);

  // Own Escape in the capture phase so it can never reach ChatPanel's
  // document-level listener, which would interrupt a running agent turn.
  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      if (e.key !== "Escape") return;
      e.preventDefault();
      e.stopPropagation();
      onClose();
    }
    document.addEventListener("keydown", onKey, true);
    return () => document.removeEventListener("keydown", onKey, true);
  }, [onClose]);

  const box = bounds
    ? {
        left: bounds.x - BOX_PADDING,
        top: bounds.y - BOX_PADDING,
        width: bounds.width + BOX_PADDING * 2,
        height: bounds.height + BOX_PADDING * 2,
      }
    : null;

  return createPortal(
    <div className="tour-overlay">
      {box ? (
        <>
          {/* Dim everything except the spotlight via an oversized box-shadow. */}
          <div className="tour-spotlight" style={box} />
          <div className="tour-ring" style={box} />
        </>
      ) : (
        <div className="tour-dim" />
      )}
      <TourCard
        step={step}
        bounds={bounds}
        index={index}
        onBack={() => setIndex((i) => Math.max(0, i - 1))}
        onNext={() => (index + 1 >= STEPS.length ? onClose() : setIndex(index + 1))}
        onClose={onClose}
      />
    </div>,
    document.body,
  );
}

function TourCard({
  step,
  bounds,
  index,
  onBack,
  onNext,
  onClose,
}: {
  step: TourStep;
  bounds: Bounds;
  index: number;
  onBack: () => void;
  onNext: () => void;
  onClose: () => void;
}) {
  const measure = useMeasure();
  const popover = usePopoverPosition(
    bounds && step.anchor
      ? {
          x: bounds.x - BOX_PADDING,
          y: bounds.y - BOX_PADDING,
          width: bounds.width + BOX_PADDING * 2,
          height: bounds.height + BOX_PADDING * 2,
          anchor: step.anchor,
          distance: CARD_DISTANCE,
        }
      : null,
    measure,
  );

  // Only trust the computed position once the card has real dimensions and
  // the viewport size is known; until then, center it.
  const positioned = step.anchor != null && bounds != null && popover.x > 0;
  const last = index + 1 === STEPS.length;

  return (
    <div
      ref={measure.ref}
      className={`tour-card ${positioned ? "" : "centered"}`}
      style={positioned ? { left: popover.x, top: popover.y } : undefined}
    >
      {positioned && step.anchor && (
        <Arrow anchor={step.anchor} adjustment={popover.arrowAdjustment} />
      )}
      <button className="icon-btn tour-close" title="Skip tour" onClick={onClose}>
        <X size={15} />
      </button>
      <h3>{step.title}</h3>
      <p>{step.description}</p>
      <div className="tour-footer">
        <div className="tour-footer-side">
          {index > 0 && (
            <button className="btn ghost" onClick={onBack}>
              Back
            </button>
          )}
        </div>
        <span className="tour-count">
          {index + 1} / {STEPS.length}
        </span>
        <div className="tour-footer-side end">
          <button className="btn primary" onClick={onNext}>
            {last ? "Done" : "Next"}
          </button>
        </div>
      </div>
    </div>
  );
}

/**
 * A rotated-square arrow on the card edge nearest the spotlight. `adjustment`
 * is how far viewport clamping displaced the card along its cross axis; the
 * arrow shifts by the same amount to keep pointing at the target.
 */
function Arrow({ anchor, adjustment }: { anchor: TourAnchor; adjustment: number }) {
  const cross =
    anchor === "above" || anchor === "below" ? `${adjustment}px 0` : `0 ${adjustment}px`;
  return <div className={`tour-arrow ${anchor}`} style={{ translate: cross }} />;
}
