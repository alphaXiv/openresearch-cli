import { CircleStop, History, RotateCw } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import {
  cancelRun,
  getCommitDiff,
  getWorkingTree,
  listExperimentCommits,
  timeAgo,
  type CommitInfo,
  type DiffPayload,
  type Experiment,
  type Run,
  type WorkingTree,
} from "../api";
import { GitDiff, TruncatedDiffNotice } from "./GitDiff";
import { LogTerminal } from "./LogTerminal";
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

export type ExperimentView = "terminal" | "changes";

/** An experiment's detail view, rendered as right-pane tab content. Mount it
 *  keyed by `${experiment.id}:${view}` so per-view state resets on switch. */
export function DetailDrawer({
  experiment,
  view,
  runs,
  selectedRunId,
  onSelectRun,
}: {
  experiment: Experiment;
  view: ExperimentView;
  runs: Run[];
  selectedRunId: string | null;
  onSelectRun: (id: string | null) => void;
}) {
  const expRuns = runs
    .filter((r) => r.experimentId === experiment.id)
    .sort((a, b) => b.createdAt - a.createdAt);

  return view === "terminal" ? (
    <TerminalView
      experiment={experiment}
      expRuns={expRuns}
      selectedRunId={selectedRunId}
      onSelectRun={onSelectRun}
    />
  ) : (
    <ChangesView experiment={experiment} />
  );
}

/**
 * A run's terminal output filling the whole pane. The bar above carries the
 * stop button, the run's status and a history switcher — mirror of
 * openresearch.sh's ExperimentFullView TerminalView.
 */
function TerminalView({
  experiment,
  expRuns,
  selectedRunId,
  onSelectRun,
}: {
  experiment: Experiment;
  expRuns: Run[];
  selectedRunId: string | null;
  onSelectRun: (id: string | null) => void;
}) {
  const [error, setError] = useState<string | null>(null);
  const [historyOpen, setHistoryOpen] = useState(false);
  const historyRef = useRef<HTMLDivElement>(null);

  const selectedRun =
    (selectedRunId && expRuns.find((r) => r.id === selectedRunId)) || expRuns[0] || null;
  const live = selectedRun?.status === "running" || selectedRun?.status === "starting";

  // When a new run starts while the tab is open, follow it live.
  const seenRunIds = useRef<Set<string> | null>(null);
  useEffect(() => {
    if (seenRunIds.current === null) {
      seenRunIds.current = new Set(expRuns.map((r) => r.id));
      return;
    }
    const fresh = expRuns.find((r) => !seenRunIds.current!.has(r.id));
    for (const r of expRuns) seenRunIds.current.add(r.id);
    if (fresh) onSelectRun(fresh.id);
  }, [expRuns, onSelectRun]);

  // Close the history dropdown on outside click.
  useEffect(() => {
    if (!historyOpen) return;
    const onDown = (e: MouseEvent) => {
      if (!historyRef.current?.contains(e.target as Node)) setHistoryOpen(false);
    };
    document.addEventListener("mousedown", onDown);
    return () => document.removeEventListener("mousedown", onDown);
  }, [historyOpen]);

  async function stop() {
    if (!selectedRun) return;
    setError(null);
    try {
      await cancelRun(selectedRun.id);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  return (
    <div className="term-view">
      <div className="term-bar">
        <div className="term-branch">
          <span className="k">branch:</span>
          <span className="name">{experiment.branchName}</span>
        </div>
        <span style={{ flex: 1 }} />
        {error && <span className="error">{error}</span>}
        {live && (
          <button className="btn sm ghost" onClick={() => void stop()}>
            <CircleStop size={13} />
            Stop
          </button>
        )}
        {selectedRun && <StatusBadge status={selectedRun.status} />}
        {expRuns.length > 0 && (
          <div className="run-history" ref={historyRef}>
            <button
              className="icon-btn"
              title="Run history"
              onClick={() => setHistoryOpen((v) => !v)}
            >
              <History size={14} />
            </button>
            {historyOpen && (
              <div className="history-menu">
                {expRuns.map((r, i) => (
                  <button
                    key={r.id}
                    className={`history-item ${r.id === selectedRun?.id ? "active" : ""}`}
                    onClick={() => {
                      onSelectRun(r.id);
                      setHistoryOpen(false);
                    }}
                  >
                    <span className="run-label">Run {expRuns.length - i}</span>
                    <StatusBadge status={r.status} />
                    <span className="when">{timeAgo(r.createdAt)}</span>
                  </button>
                ))}
              </div>
            )}
          </div>
        )}
      </div>

      <div className="term-fill">
        {selectedRun ? (
          // Key by run id so switching runs in the history dropdown remounts
          // the terminal with the selected run's output.
          <LogTerminal key={selectedRun.id} runId={selectedRun.id} />
        ) : (
          <div className="term-empty">No runs yet — ask the agent to launch one.</div>
        )}
      </div>
    </div>
  );
}

/** The branch's changes: a commit picker + diff, including uncommitted edits. */
function ChangesView({ experiment }: { experiment: Experiment }) {
  const [commits, setCommits] = useState<CommitInfo[] | null>(null);
  const [changesError, setChangesError] = useState<string | null>(null);
  const [workingTree, setWorkingTree] = useState<WorkingTree | null>(null);
  const [selection, setSelection] = useState<string | null>(null);
  const [commitDiffs, setCommitDiffs] = useState<Record<string, DiffState>>({});

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

  // First open loads commits + working tree.
  useEffect(() => {
    if (commits === null && !changesError) void loadChanges();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [commits, changesError, experiment.id]);

  // Poll the working tree while the view is visible.
  useEffect(() => {
    const timer = setInterval(() => {
      getWorkingTree(experiment.projectId)
        .then(setWorkingTree)
        .catch(() => {
          // transient; next tick retries
        });
    }, 5000);
    return () => clearInterval(timer);
  }, [experiment.projectId]);

  // If the uncommitted diff disappears from under the selection, fall back.
  useEffect(() => {
    if (selection === UNCOMMITTED && workingTree && !uncommittedAvailable) {
      setSelection(commits?.[0]?.sha ?? null);
    }
  }, [selection, workingTree, uncommittedAvailable, commits]);

  // Lazily fetch the selected commit's diff, cached per sha.
  useEffect(() => {
    if (!selection || selection === UNCOMMITTED) return;
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
  }, [selection, commitDiffs, experiment.id]);

  const noChanges = !uncommittedAvailable && (commits?.length ?? 0) === 0;

  return (
    <div className="drawer">
      <div className="drawer-body">
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
      </div>
    </div>
  );
}
