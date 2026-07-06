import { RotateCw } from "lucide-react";
import { useEffect, useState } from "react";
import {
  cancelRun,
  getCommitDiff,
  getRunDiff,
  getWorkingTree,
  listExperimentCommits,
  shortId,
  timeAgo,
  type CommitInfo,
  type DiffPayload,
  type Experiment,
  type Run,
  type WorkingTree,
} from "../api";
import { GitDiff, TruncatedDiffNotice } from "./GitDiff";
import { LogTerminal } from "./LogTerminal";
import { Md } from "./Md";
import { StatusBadge } from "./StatusBadge";

const UNCOMMITTED = "__uncommitted__";

interface DiffState {
  loading: boolean;
  error?: string;
  payload?: DiffPayload;
}

function DiffView({ state }: { state: DiffState | undefined }) {
  if (!state || state.loading) return <div className="changes-note">Loading diff…</div>;
  if (state.error) return <div className="error">{state.error}</div>;
  if (!state.payload) return <div className="diff-empty">No changes.</div>;
  if (state.payload.truncated) {
    return (
      <TruncatedDiffNotice
        bytesRead={state.payload.bytesRead}
        byteLimit={state.payload.byteLimit}
      />
    );
  }
  return <GitDiff diff={state.payload.diff} />;
}

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
  const [error, setError] = useState<string | null>(null);

  const [drawerTab, setDrawerTab] = useState<"overview" | "changes">("overview");
  const [runTab, setRunTab] = useState<"log" | "diff" | "result">("log");
  const [runDiffs, setRunDiffs] = useState<Record<string, DiffState>>({});

  // Changes tab state
  const [commits, setCommits] = useState<CommitInfo[] | null>(null);
  const [changesError, setChangesError] = useState<string | null>(null);
  const [workingTree, setWorkingTree] = useState<WorkingTree | null>(null);
  const [selection, setSelection] = useState<string | null>(null);
  const [commitDiffs, setCommitDiffs] = useState<Record<string, DiffState>>({});

  const expRuns = runs
    .filter((r) => r.experimentId === experiment.id)
    .sort((a, b) => b.createdAt - a.createdAt);
  const selectedRun =
    (selectedRunId && expRuns.find((r) => r.id === selectedRunId)) || expRuns[0] || null;
  const live = selectedRun?.status === "running" || selectedRun?.status === "starting";

  // Diff only exists for child-experiment runs that produced a commit.
  const diffAvailable = Boolean(experiment.parentExperimentId && selectedRun?.commitSha);
  const resultAvailable = Boolean(selectedRun?.resultMarkdown);
  const activeRunTab =
    (runTab === "diff" && !diffAvailable) || (runTab === "result" && !resultAvailable)
      ? "log"
      : runTab;

  // Reset per-experiment state when the drawer switches experiments.
  useEffect(() => {
    setDrawerTab("overview");
    setRunTab("log");
    setRunDiffs({});
    setCommits(null);
    setChangesError(null);
    setWorkingTree(null);
    setSelection(null);
    setCommitDiffs({});
  }, [experiment.id]);

  // Lazily fetch the selected run's diff, cached per run id.
  useEffect(() => {
    if (drawerTab !== "overview" || activeRunTab !== "diff" || !selectedRun) return;
    const runId = selectedRun.id;
    if (runDiffs[runId]) return;
    setRunDiffs((m) => ({ ...m, [runId]: { loading: true } }));
    getRunDiff(runId)
      .then((payload) => setRunDiffs((m) => ({ ...m, [runId]: { loading: false, payload } })))
      .catch((err) =>
        setRunDiffs((m) => ({
          ...m,
          [runId]: { loading: false, error: err instanceof Error ? err.message : String(err) },
        })),
      );
  }, [drawerTab, activeRunTab, selectedRun, runDiffs]);

  const uncommittedAvailable = Boolean(
    workingTree &&
      workingTree.diff.trim() !== "" &&
      workingTree.experimentId === experiment.id,
  );

  async function loadChanges() {
    setChangesError(null);
    try {
      const [commitList, wt] = await Promise.all([
        listExperimentCommits(experiment.id),
        getWorkingTree(experiment.projectId),
      ]);
      setCommits(commitList);
      setWorkingTree(wt);
      setSelection((prev) => {
        if (prev !== null) return prev;
        const wtActive = wt.diff.trim() !== "" && wt.experimentId === experiment.id;
        if (wtActive) return UNCOMMITTED;
        return commitList[0]?.sha ?? null;
      });
    } catch (err) {
      setChangesError(err instanceof Error ? err.message : String(err));
    }
  }

  // First open of the Changes tab loads commits + working tree.
  useEffect(() => {
    if (drawerTab === "changes" && commits === null && !changesError) void loadChanges();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [drawerTab, commits, changesError, experiment.id]);

  // Poll the working tree while the Changes tab is visible.
  useEffect(() => {
    if (drawerTab !== "changes") return;
    const timer = setInterval(() => {
      getWorkingTree(experiment.projectId)
        .then(setWorkingTree)
        .catch(() => {
          // transient; next tick retries
        });
    }, 5000);
    return () => clearInterval(timer);
  }, [drawerTab, experiment.projectId]);

  // If the uncommitted diff disappears from under the selection, fall back.
  useEffect(() => {
    if (selection === UNCOMMITTED && workingTree && !uncommittedAvailable) {
      setSelection(commits?.[0]?.sha ?? null);
    }
  }, [selection, workingTree, uncommittedAvailable, commits]);

  // Lazily fetch the selected commit's diff, cached per sha.
  useEffect(() => {
    if (drawerTab !== "changes" || !selection || selection === UNCOMMITTED) return;
    if (commitDiffs[selection]) return;
    const sha = selection;
    setCommitDiffs((m) => ({ ...m, [sha]: { loading: true } }));
    getCommitDiff(experiment.id, sha)
      .then((payload) => setCommitDiffs((m) => ({ ...m, [sha]: { loading: false, payload } })))
      .catch((err) =>
        setCommitDiffs((m) => ({
          ...m,
          [sha]: { loading: false, error: err instanceof Error ? err.message : String(err) },
        })),
      );
  }, [drawerTab, selection, commitDiffs, experiment.id]);

  async function cancel() {
    if (!selectedRun) return;
    setError(null);
    try {
      await cancelRun(selectedRun.id);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  const overview = (
    <>
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
            No runs yet — ask the agent to launch one.
          </div>
        ) : (
          expRuns.map((run) => (
            <button
              key={run.id}
              className={`run-row ${selectedRun?.id === run.id ? "active" : ""}`}
              onClick={() => onSelectRun(run.id)}
            >
              <StatusBadge status={run.status} />
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
            Run — {shortId(selectedRun.id)}
            {live ? " (live)" : ""}
          </h3>
          <div className="run-tabs">
            <button
              className={activeRunTab === "log" ? "active" : ""}
              onClick={() => setRunTab("log")}
            >
              Log
            </button>
            {diffAvailable && (
              <button
                className={activeRunTab === "diff" ? "active" : ""}
                onClick={() => setRunTab("diff")}
              >
                Diff
              </button>
            )}
            {resultAvailable && (
              <button
                className={activeRunTab === "result" ? "active" : ""}
                onClick={() => setRunTab("result")}
              >
                Result
              </button>
            )}
          </div>

          {activeRunTab === "log" && (
            <>
              <div className="terminal-box">
                <LogTerminal key={selectedRun.id} runId={selectedRun.id} />
              </div>
              {live && (
                <button
                  className="btn sm danger"
                  style={{ marginTop: 8 }}
                  onClick={() => void cancel()}
                >
                  Cancel
                </button>
              )}
            </>
          )}
          {activeRunTab === "diff" && <DiffView state={runDiffs[selectedRun.id]} />}
          {activeRunTab === "result" && selectedRun.resultMarkdown && (
            <Md text={selectedRun.resultMarkdown} />
          )}
        </div>
      )}
    </>
  );

  const noChanges = !uncommittedAvailable && (commits?.length ?? 0) === 0;

  const changes = (
    <div className="drawer-section">
      {changesError ? (
        <div className="error">{changesError}</div>
      ) : commits === null ? (
        <div className="changes-note">Loading changes…</div>
      ) : noChanges ? (
        <div className="changes-note">
          No changes yet — the agent hasn't committed on this branch.
        </div>
      ) : (
        <>
          <div className="commit-picker">
            {selection === UNCOMMITTED && <span className="uncommitted-dot" />}
            <select
              className="input sm"
              value={selection ?? ""}
              onChange={(e) => setSelection(e.target.value)}
            >
              {uncommittedAvailable && (
                <option value={UNCOMMITTED}>● Uncommitted changes</option>
              )}
              {commits.map((c) => (
                <option key={c.sha} value={c.sha}>
                  {c.sha.slice(0, 7)} — {c.subject}
                </option>
              ))}
            </select>
            <button
              className="btn sm ghost"
              title="Refresh"
              onClick={() => void loadChanges()}
            >
              <RotateCw size={13} />
            </button>
          </div>
          <div style={{ marginTop: 10 }}>
            {selection === UNCOMMITTED && workingTree ? (
              workingTree.truncated ? (
                <div className="truncated-notice">
                  <h4>Diff too large to display</h4>
                  <p>The uncommitted diff is too large to display. View it locally with git.</p>
                </div>
              ) : (
                <GitDiff diff={workingTree.diff} />
              )
            ) : selection ? (
              <DiffView state={commitDiffs[selection]} />
            ) : (
              <div className="changes-note">Select a commit to view its diff.</div>
            )}
          </div>
        </>
      )}
    </div>
  );

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
        <button className="close" onClick={onClose} title="Close">
          ×
        </button>
      </div>

      <div className="drawer-tabs">
        <button
          className={`tab ${drawerTab === "overview" ? "active" : ""}`}
          onClick={() => setDrawerTab("overview")}
        >
          Overview
        </button>
        <button
          className={`tab ${drawerTab === "changes" ? "active" : ""}`}
          onClick={() => setDrawerTab("changes")}
        >
          Changes
        </button>
      </div>

      <div className="drawer-body">
        {error && (
          <div className="form">
            <div className="error">{error}</div>
          </div>
        )}
        {drawerTab === "overview" ? overview : changes}
      </div>
    </div>
  );
}
