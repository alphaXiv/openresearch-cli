// Mirror of openresearch.sh StatusBadge: mono uppercase label + colored dot,
// live statuses pulse. STATUS_STYLES is the single source of truth for status
// coloring across the table, graph and drawer.

export interface StatusStyle {
  className: string;
  live: boolean;
}

export const STATUS_STYLES: Record<string, StatusStyle> = {
  done: { className: "st-done", live: false },
  failed: { className: "st-failed", live: false },
  running: { className: "st-running", live: true },
  starting: { className: "st-starting", live: true },
  cancelled: { className: "st-cancelled", live: false },
  editing: { className: "st-editing", live: true },
  idle: { className: "st-idle", live: false },
};

export function statusStyle(status: string): StatusStyle {
  return STATUS_STYLES[status] ?? STATUS_STYLES.idle;
}

export function StatusBadge({ status, label }: { status: string; label?: string }) {
  const style = statusStyle(status);
  return (
    <span className={`status-badge ${style.className}${style.live ? " live" : ""}`}>
      <span className="dot" />
      {label ?? status}
    </span>
  );
}
