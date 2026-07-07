import { Home, Settings } from "lucide-react";

/** Brand row at the top of the agents rail (the old full-width header,
 *  folded into the left pane). Project switching lives on the home page. */
export function RailHeader({
  onHome,
  onOpenSettings,
}: {
  onHome: () => void;
  onOpenSettings: () => void;
}) {
  return (
    <div className="rail-brand">
      <button className="icon-btn" title="Projects" aria-label="Projects" onClick={onHome}>
        <Home size={15} />
      </button>
      <button className="brand" onClick={onHome} title="Projects">
        Open<span>Research</span>
      </button>
      <span style={{ flex: 1 }} />
      <button
        className="icon-btn"
        title="Settings"
        aria-label="Settings"
        onClick={onOpenSettings}
      >
        <Settings size={15} />
      </button>
    </div>
  );
}
