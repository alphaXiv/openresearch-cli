import { statusColor, shortId, timeAgo, type Experiment, type Run } from "../api";

export function StatusChip({ status }: { status: string }) {
  return (
    <span className="status-chip" style={{ color: statusColor(status) }}>
      <span className="dot" style={{ background: statusColor(status) }} />
      {status}
    </span>
  );
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
  onCancel,
}: {
  runs: Run[];
  experiments: Experiment[];
  onOpen: (run: Run) => void;
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
                  <StatusChip status={run.status} />
                </td>
                <td className="mono">{backendLabel(run)}</td>
                <td className="mono">{run.commitSha ? run.commitSha.slice(0, 7) : "—"}</td>
                <td>{timeAgo(run.createdAt)}</td>
                <td className="mono">{run.exitCode ?? "—"}</td>
                <td>
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
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}
