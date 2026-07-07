import { Check, ChevronDown } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { getHarnesses, modelLabel, type Harness, type HarnessId } from "../api";

export interface ModelSelection {
  harness: HarnessId;
  model: string | null; // null = the harness's default model
}

export const HARNESS_LABELS: Record<HarnessId, string> = {
  "claude-code": "Claude Code",
  codex: "Codex",
  opencode: "OpenCode",
};

/** First harness that can actually run — the fallback when nothing is picked. */
export function defaultSelection(harnesses: Harness[]): ModelSelection | null {
  const ready = harnesses.find((h) => h.agentReady);
  return ready ? { harness: ready.id, model: ready.models[0]?.id ?? null } : null;
}

/** Composer selector: pick the harness + model new sessions (and same-harness
 * turns) run on. Groups mirror the Harnesses settings tab. */
export function ModelPicker({
  value,
  onSelect,
  onHarnesses,
}: {
  value: ModelSelection | null;
  onSelect: (value: ModelSelection) => void;
  onHarnesses?: (harnesses: Harness[]) => void;
}) {
  const [harnesses, setHarnesses] = useState<Harness[]>([]);
  const [open, setOpen] = useState(false);
  const [filter, setFilter] = useState("");
  const rootRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    getHarnesses()
      .then((list) => {
        setHarnesses(list);
        onHarnesses?.(list);
      })
      .catch(() => {});
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Close on outside click.
  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (!rootRef.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", onDown);
    return () => document.removeEventListener("mousedown", onDown);
  }, [open]);

  const groups = useMemo(() => {
    const q = filter.trim().toLowerCase();
    return harnesses.map((h) => {
      let models = h.models;
      if (q) models = models.filter((m) => m.id.toLowerCase().includes(q));
      // opencode's long tail (openrouter etc.) stays behind the filter box.
      else if (h.id === "opencode") models = models.slice(0, 6);
      return { harness: h, models, hidden: q ? 0 : h.models.length - models.length };
    });
  }, [harnesses, filter]);

  const pick = (harness: HarnessId, model: string | null) => {
    onSelect({ harness, model });
    setOpen(false);
    setFilter("");
  };

  const label = value
    ? `${HARNESS_LABELS[value.harness]} · ${value.model ? modelLabel(value.model) : "default"}`
    : "Model";

  return (
    <div className="model-picker" ref={rootRef}>
      <button
        type="button"
        className="model-btn"
        title="Harness + model for this chat"
        onClick={() => setOpen((v) => !v)}
      >
        {label}
        <ChevronDown size={12} />
      </button>
      {open && (
        <div className="model-menu">
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
                        onClick={() => pick(harness.id, m.id)}
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
        </div>
      )}
    </div>
  );
}
