import { Check, ChevronDown, Lock } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import {
  getHarnesses,
  modelLabel,
  type Harness,
  type HarnessId,
  type OptionChoice,
} from "../api";

export interface ModelSelection {
  harness: HarnessId;
  model: string | null; // null = the harness's default model
  /** Permission-mode wire id (null = harness default). */
  permissionMode: string | null;
  /** Reasoning-level wire id (null = harness default). */
  reasoningLevel: string | null;
}

export const HARNESS_LABELS: Record<HarnessId, string> = {
  "claude-code": "Claude Code",
  codex: "Codex",
  opencode: "OpenCode",
};

/** First harness that can actually run — the fallback when nothing is picked.
 * Seeds mode/reasoning from that harness's advertised defaults. */
export function defaultSelection(harnesses: Harness[]): ModelSelection | null {
  const ready = harnesses.find((h) => h.agentReady);
  if (!ready) return null;
  return {
    harness: ready.id,
    model: ready.models[0]?.id ?? null,
    permissionMode: ready.options?.defaultPermissionMode ?? null,
    reasoningLevel: ready.options?.defaultReasoningLevel ?? null,
  };
}

/** Close-on-outside-click + open state shared by the composer dropdowns (and
 * the session-rail menus). */
export function usePopover() {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      // Escape closes the picker and must NOT bubble to other document-level
      // Escape handlers (e.g. ChatPanel's stop-streaming listener) — closing an
      // open picker shouldn't also interrupt an in-flight turn. Capture phase +
      // stopPropagation makes this order-independent: relying on registration
      // order and defaultPrevented among bubble-phase document listeners is
      // fragile, since whichever registered first runs first.
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", onDown);
    document.addEventListener("keydown", onKey, true);
    return () => {
      document.removeEventListener("mousedown", onDown);
      document.removeEventListener("keydown", onKey, true);
    };
  }, [open]);
  return { open, setOpen, ref };
}

/** Composer selector: pick the harness + model new sessions (and same-harness
 * turns) run on. Groups mirror the Harnesses settings tab. */
export function ModelPicker({
  value,
  onSelect,
  onHarnesses,
  lockHarness = false,
}: {
  value: ModelSelection | null;
  onSelect: (value: ModelSelection) => void;
  onHarnesses?: (harnesses: Harness[]) => void;
  /** When set (a session is open), only the current harness is offered — its
   * harness is fixed for its lifetime, so you can still switch models within it
   * but not switch to a different harness. */
  lockHarness?: boolean;
}) {
  const [harnesses, setHarnesses] = useState<Harness[]>([]);
  const { open, setOpen, ref: rootRef } = usePopover();
  const [filter, setFilter] = useState("");

  useEffect(() => {
    getHarnesses()
      .then((list) => {
        setHarnesses(list);
        onHarnesses?.(list);
      })
      .catch(() => {});
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const groups = useMemo(() => {
    const q = filter.trim().toLowerCase();
    // Locked to the open session's harness: only offer that one.
    const shown =
      lockHarness && value ? harnesses.filter((h) => h.id === value.harness) : harnesses;
    return shown.map((h) => {
      let models = h.models;
      if (q) models = models.filter((m) => m.id.toLowerCase().includes(q));
      // opencode's long tail (openrouter etc.) stays behind the filter box.
      else if (h.id === "opencode") models = models.slice(0, 6);
      return { harness: h, models, hidden: q ? 0 : h.models.length - models.length };
    });
  }, [harnesses, filter, lockHarness, value]);

  /** Switch harness → reseed model + mode/reasoning defaults for that harness. */
  const pick = (harness: Harness, model: string | null) => {
    const sameHarness = value?.harness === harness.id;
    onSelect({
      harness: harness.id,
      model,
      permissionMode: sameHarness
        ? value!.permissionMode
        : harness.options?.defaultPermissionMode ?? null,
      reasoningLevel: sameHarness
        ? value!.reasoningLevel
        : harness.options?.defaultReasoningLevel ?? null,
    });
    setOpen(false);
    setFilter("");
  };

  const label = value
    ? value.model
      ? modelLabel(value.model)
      : "Default model"
    : "Model";

  return (
    <div className="model-picker" ref={rootRef}>
      <button
        type="button"
        className="composer-pill"
        title="Harness + model for this chat"
        onClick={() => setOpen((v) => !v)}
      >
        {label}
        <ChevronDown size={12} />
      </button>
      {open && (
        <div className="model-menu align-right">
          <input
            autoFocus
            type="text"
            placeholder="Search models…"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
          />
          <div className="model-menu-list">
            {groups.map(({ harness, models, hidden }) => (
              <div key={harness.id}>
                <div className="model-group">{harness.name}</div>
                {!harness.agentReady ? (
                  <div className="model-more">{harness.agentNote ?? "Not available"}</div>
                ) : (
                  <>
                    {models.map((m) => (
                      <button
                        key={m.id}
                        className="model-item"
                        onClick={() => pick(harness, m.id)}
                      >
                        <span>
                          {modelLabel(m.id)}
                          <span className="model-id">{m.id}</span>
                        </span>
                        {value?.harness === harness.id && value?.model === m.id && (
                          <Check size={13} />
                        )}
                      </button>
                    ))}
                    {hidden > 0 && (
                      <div className="model-more">{hidden} more — search to find</div>
                    )}
                  </>
                )}
              </div>
            ))}
            {harnesses.length === 0 && <div className="model-more">Detecting harnesses…</div>}
          </div>
          {lockHarness && value && harnesses.length > 1 && (
            <div className="model-locked-note">
              <Lock size={11} />
              Sessions keep their harness — new chat to switch
            </div>
          )}
        </div>
      )}
    </div>
  );
}

/** A compact single-axis dropdown (permission mode or reasoning level). Renders
 * nothing when the harness advertises no choices for this axis. Mirrors the
 * Claude Code composer menu: a header, the current-default row pinned at top
 * with a "· Default" note, then the full numbered list. */
export function OptionPicker({
  choices,
  value,
  defaultId,
  header,
  align = "left",
  variant = "pill",
  title,
  numbered = false,
  onSelect,
}: {
  choices: OptionChoice[];
  value: string | null;
  /** The harness's default id — pinned at top of the menu and used when
   * `value` is null. */
  defaultId?: string | null;
  /** Uppercase group header (e.g. "Mode"). */
  header?: string;
  align?: "left" | "right";
  /** `pill` = boxed (permission mode); `bare` = text-only (reasoning). */
  variant?: "pill" | "bare";
  title?: string;
  /** Show 1-based number hints on the right (like the mode menu). */
  numbered?: boolean;
  onSelect: (id: string) => void;
}) {
  const { open, setOpen, ref } = usePopover();
  if (choices.length === 0) return null;

  const effectiveId = value ?? defaultId ?? choices[0]?.id ?? null;
  const current = choices.find((c) => c.id === effectiveId);
  const defaultChoice = choices.find((c) => c.id === defaultId);
  const label = current?.label ?? choices[0]?.label ?? "";

  const choose = (id: string) => {
    onSelect(id);
    setOpen(false);
  };

  return (
    <div className="option-picker" ref={ref}>
      <button
        type="button"
        className={variant === "pill" ? "composer-pill" : "composer-bare"}
        title={title}
        onClick={() => setOpen((v) => !v)}
      >
        {label}
        <ChevronDown size={12} />
      </button>
      {open && (
        <div className={`option-menu ${align === "right" ? "align-right" : ""}`}>
          {header && <div className="model-group">{header}</div>}
          {defaultChoice && (
            <>
              <button className="model-item" onClick={() => choose(defaultChoice.id)}>
                <span>
                  {defaultChoice.label} <span className="option-default">· Default</span>
                </span>
                {effectiveId === defaultChoice.id && <Check size={13} />}
              </button>
              <div className="option-sep" />
            </>
          )}
          {choices.map((c, i) => (
            <button key={c.id} className="model-item" onClick={() => choose(c.id)}>
              <span>{c.label}</span>
              {effectiveId === c.id ? (
                <Check size={13} />
              ) : (
                numbered && <span className="option-num">{i + 1}</span>
              )}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
