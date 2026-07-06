import { GitBranch } from "lucide-react";
import { shortId, timeAgo, type Experiment, type Run } from "../api";
import { StatusBadge } from "./StatusBadge";

// Legacy alias kept for external imports; StatusBadge owns the styling.
export function StatusChip({ status }: { status: string }) {
  return <StatusBadge status={status} />;
}

function backendLabel(run: Run): string {
  const b = run.backend;
  if (!b) return "—";
  const kind = typeof b.kind === "string" ? b.kind : typeof b.type === "string" ? b.type : "";
  const flavor = typeof b.flavor === "string" ? b.flavor : "";
  return [kind, flavor].filter(Boolean).join(" · ") || "—";
}

export function RunsTable({
  runs,
  experiments,
  onOpen,
  onOpenChanges,
  onCancel,
}: {
  runs: Run[];
  experiments: Experiment[];
  /** Row click — opens the run's experiment terminal tab. */
  onOpen: (run: Run) => void;
  /** GitBranch shortcut — opens the experiment's changes tab. */
  onOpenChanges: (experimentId: string) => void;
  onCancel: (runId: string) => void;
}) {
  const slugByExp = new Map(experiments.map((e) => [e.id, e.slug]));
  const sorted = [...runs].sort((a, b) => b.createdAt - a.createdAt);

  if (sorted.length === 0) {
    return (
      <div className="empty-state">
        <p>No runs yet.</p>
      </div>
    );
  }

  return (
    <div className="runs-table-wrap">
      <table className="runs-table">
        <thead>
          <tr>
            <th>Run</th>
            <th>Experiment</th>
            <th>Status</th>
            <th>Backend</th>
            <th>Commit</th>
            <th>Started</th>
            <th>Exit</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {sorted.map((run) => {
            const live = run.status === "running" || run.status === "starting";
            return (
              <tr key={run.id} className="clickable" onClick={() => onOpen(run)}>
                <td className="mono">{shortId(run.id)}</td>
                <td className="mono">{slugByExp.get(run.experimentId) ?? shortId(run.experimentId)}</td>
                <td>
                  <StatusBadge status={run.status} />
                </td>
                <td className="mono">{backendLabel(run)}</td>
                <td className="mono">{run.commitSha ? run.commitSha.slice(0, 7) : "—"}</td>
                <td>{timeAgo(run.createdAt)}</td>
                <td className="mono">{run.exitCode ?? "—"}</td>
                <td>
                  <span style={{ display: "inline-flex", alignItems: "center", gap: 4 }}>
                    <button
                      className="icon-btn"
                      title="Open changes"
                      onClick={(e) => {
                        e.stopPropagation();
                        onOpenChanges(run.experimentId);
                      }}
                    >
                      <GitBranch size={14} />
                    </button>
                    {live && (
                      <button
                        className="btn sm danger"
                        onClick={(e) => {
                          e.stopPropagation();
                          onCancel(run.id);
                        }}
                      >
                        Cancel
                      </button>
                    )}
                  </span>
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}
