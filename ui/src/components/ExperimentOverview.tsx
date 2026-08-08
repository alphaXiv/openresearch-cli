import {
  CalendarDays,
  Clock3,
  FolderTree,
  GitBranch,
  GitCommitHorizontal,
  Terminal,
} from "lucide-react";
import { useEffect, useState } from "react";
import {
  fmtDuration,
  runDisplayStatus,
  timeAgo,
  type Experiment,
  type Run,
} from "../api";
import { BackendBadge } from "./BackendLogos";
import { BranchPill } from "./BranchPill";
import { Md } from "./Md";
import { StatusBadge } from "./StatusBadge";

function fmtDate(ms: number): string {
  return new Date(ms).toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    year: "numeric",
    hour: "numeric",
    minute: "2-digit",
  });
}

function runDuration(run: Run, now: number): string {
  return fmtDuration((run.endedAt ?? now) - run.createdAt);
}

export function ExperimentOverview({
  experiment,
  parentExperiment,
  runs,
  onOpenLogs,
  onOpenChanges,
  onOpenCode,
}: {
  experiment: Experiment;
  parentExperiment: Experiment | null;
  runs: Run[];
  onOpenLogs: (runId: string) => void;
  onOpenChanges: () => void;
  onOpenCode: () => void;
}) {
  const latestRun = runs[0] ?? null;
  const hasLiveRun = runs.some(
    (run) => run.status === "running" || run.status === "starting",
  );
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    if (!hasLiveRun) return;
    setNow(Date.now());
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [hasLiveRun]);

  return (
    <div className="experiment-overview">
      <div className="experiment-overview-inner">
        <header className="experiment-overview-head">
          <div className="experiment-overview-heading">
            <h1>{experiment.title || experiment.slug}</h1>
            <div className="experiment-overview-slug">{experiment.slug}</div>
          </div>
          <StatusBadge status={latestRun ? runDisplayStatus(latestRun) : "idle"} />
        </header>

        <div className="experiment-overview-actions">
          {latestRun && (
            <button
              className="experiment-overview-action"
              onClick={() => onOpenLogs(latestRun.id)}
            >
              <Terminal size={15} />
              Logs
            </button>
          )}
          <button className="experiment-overview-action" onClick={onOpenChanges}>
            <GitBranch size={15} />
            Changes
          </button>
          <button className="experiment-overview-action" onClick={onOpenCode}>
            <FolderTree size={15} />
            Code
          </button>
        </div>

        {experiment.description && (
          <section className="experiment-overview-section overview-description">
            <h2>Description</h2>
            <Md text={experiment.description} />
          </section>
        )}

        <section className="experiment-overview-section">
          <h2>{latestRun ? "Latest run" : "Runs"}</h2>
          {latestRun && (
            <>
              <div className="experiment-overview-meta">
                <StatusBadge status={runDisplayStatus(latestRun)} />
                <BackendBadge backend={latestRun.backend} />
                <span title="Started">
                  <CalendarDays size={13} />
                  {fmtDate(latestRun.createdAt)}
                </span>
                <span title="Duration">
                  <Clock3 size={13} />
                  {runDuration(latestRun, now)}
                </span>
                {latestRun.commitSha && (
                  <span title="Commit">
                    <GitCommitHorizontal size={14} />
                    <code>{latestRun.commitSha.slice(0, 7)}</code>
                  </span>
                )}
                {latestRun.exitCode !== null &&
                  latestRun.exitCode !== undefined &&
                  latestRun.exitCode !== 0 && (
                    <span>exit {latestRun.exitCode}</span>
                  )}
              </div>
              {latestRun.command && (
                <code className="experiment-overview-command">$ {latestRun.command}</code>
              )}
              {latestRun.resultMarkdown && (
                <div
                  className={`experiment-overview-result ${latestRun.status === "failed" ? "failed" : ""}`}
                >
                  <Md text={latestRun.resultMarkdown} />
                </div>
              )}
            </>
          )}
        </section>

        <section className="experiment-overview-section">
          <h2>Git</h2>
          <div className="experiment-overview-meta experiment-overview-git-meta">
            <BranchPill branch={experiment.branchName} />
            {parentExperiment && (
              <span>
                from <code>{parentExperiment.slug}</code>
              </span>
            )}
            <span title={fmtDate(experiment.createdAt)}>
              created {timeAgo(experiment.createdAt)}
            </span>
          </div>
          {experiment.runCommand !== latestRun?.command && (
            <code className="experiment-overview-command">$ {experiment.runCommand}</code>
          )}
        </section>

        {runs.length > 0 && (
          <section className="experiment-overview-section">
            <h2>Run history</h2>
            <div className="experiment-run-history">
              {runs.map((run, index) => (
                <button key={run.id} onClick={() => onOpenLogs(run.id)}>
                  <span className="experiment-run-number">Run {runs.length - index}</span>
                  <StatusBadge status={runDisplayStatus(run)} />
                  <span>{timeAgo(run.createdAt)}</span>
                  <span>{runDuration(run, now)}</span>
                  <Terminal size={13} />
                </button>
              ))}
            </div>
          </section>
        )}
      </div>
    </div>
  );
}
