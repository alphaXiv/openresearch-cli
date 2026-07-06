import { useState } from "react";
import {
  cancelRun,
  shortId,
  startRun,
  timeAgo,
  type Experiment,
  type Run,
} from "../api";
import { LogTerminal } from "./LogTerminal";
import { Md } from "./Md";
import { StatusChip } from "./RunsTable";

const HF_FLAVORS = [
  "cpu-basic",
  "cpu-upgrade",
  "t4-small",
  "t4-medium",
  "l4x1",
  "l40sx1",
  "a10g-small",
  "a10g-large",
  "a100-large",
  "h100",
  "h200",
];

export function DetailDrawer({
  experiment,
  runs,
  selectedRunId,
  onSelectRun,
  onClose,
}: {
  experiment: Experiment;
  runs: Run[];
  selectedRunId: string | null;
  onSelectRun: (id: string | null) => void;
  onClose: () => void;
}) {
  const [launching, setLaunching] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [flavor, setFlavor] = useState(() => localStorage.getItem("orx.flavor") ?? "");

  const expRuns = runs
    .filter((r) => r.experimentId === experiment.id)
    .sort((a, b) => b.createdAt - a.createdAt);
  const selectedRun =
    (selectedRunId && expRuns.find((r) => r.id === selectedRunId)) || expRuns[0] || null;
  const live = selectedRun?.status === "running" || selectedRun?.status === "starting";

  async function launch() {
    const f = flavor.trim();
    if (!f) {
      setError("Pick an HF Jobs flavor first (e.g. a10g-small, l4x1, cpu-basic).");
      return;
    }
    localStorage.setItem("orx.flavor", f);
    setLaunching(true);
    setError(null);
    try {
      const run = await startRun(experiment.id, { flavor: f });
      onSelectRun(run.id);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setLaunching(false);
    }
  }

  async function cancel() {
    if (!selectedRun) return;
    setError(null);
    try {
      await cancelRun(selectedRun.id);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  return (
    <div className="drawer">
      <div className="drawer-header">
        <span className="slug">{experiment.slug}</span>
        {experiment.title && (
          <span style={{ color: "var(--subtext)", fontSize: 12.5, flex: 1, minWidth: 0 }}>
            {experiment.title}
          </span>
        )}
        <span style={{ flex: 1 }} />
        <input
          className="input sm"
          list="hf-flavors"
          placeholder="flavor"
          title="HF Jobs flavor (priced per minute on your HF account)"
          style={{ width: 110 }}
          value={flavor}
          onChange={(e) => setFlavor(e.target.value)}
        />
        <datalist id="hf-flavors">
          {HF_FLAVORS.map((f) => (
            <option key={f} value={f} />
          ))}
        </datalist>
        <button className="btn sm primary" onClick={() => void launch()} disabled={launching}>
          {launching ? "Launching…" : "Run"}
        </button>
        {live && (
          <button className="btn sm danger" onClick={() => void cancel()}>
            Cancel
          </button>
        )}
        <button className="close" onClick={onClose} title="Close">
          ×
        </button>
      </div>

      <div className="drawer-body">
        {error && <div className="form"><div className="error">{error}</div></div>}

        <div className="drawer-section">
          <h3>Experiment</h3>
          <div className="kv">
            <span className="k">branch</span>
            <span className="v">{experiment.branchName}</span>
            <span className="k">run command</span>
            <span className="v">{experiment.runCommand || "—"}</span>
            {experiment.description && (
              <>
                <span className="k">notes</span>
                <span className="v" style={{ fontFamily: "var(--sans)" }}>
                  {experiment.description}
                </span>
              </>
            )}
          </div>
        </div>

        <div className="drawer-section">
          <h3>Runs</h3>
          {expRuns.length === 0 ? (
            <div style={{ color: "var(--muted)", fontSize: 12.5 }}>
              Never run — hit Run to launch on HF Jobs.
            </div>
          ) : (
            expRuns.map((run) => (
              <button
                key={run.id}
                className={`run-row ${selectedRun?.id === run.id ? "active" : ""}`}
                onClick={() => onSelectRun(run.id)}
              >
                <StatusChip status={run.status} />
                <span className="mono">{shortId(run.id)}</span>
                {run.commitSha && <span className="mono">{run.commitSha.slice(0, 7)}</span>}
                <span style={{ marginLeft: "auto", color: "var(--muted)" }}>
                  {timeAgo(run.createdAt)}
                </span>
                {run.exitCode != null && <span className="mono">exit {run.exitCode}</span>}
              </button>
            ))
          )}
        </div>

        {selectedRun && (
          <div className="drawer-section">
            <h3>
              Log — {shortId(selectedRun.id)}
              {live ? " (live)" : ""}
            </h3>
            <div className="terminal-box">
              <LogTerminal key={selectedRun.id} runId={selectedRun.id} />
            </div>
          </div>
        )}

        {selectedRun?.resultMarkdown && (
          <div className="drawer-section">
            <h3>Result</h3>
            <Md text={selectedRun.resultMarkdown} />
          </div>
        )}
      </div>
    </div>
  );
}
