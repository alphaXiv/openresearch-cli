import { ArrowLeft, PanelLeft } from "lucide-react";

/** Top row of the agents rail: back to the projects page + the current
 *  project's name. Settings sections live in the rail nav below. */
export function RailHeader({
  projectName,
  onHome,
  onCollapse,
}: {
  projectName: string;
  onHome: () => void;
  /** Hide the rail (a matching reopen button lives in the chat header). */
  onCollapse?: () => void;
}) {
  return (
    <div className="rail-brand">
      <button className="brand" onClick={onHome} title="All projects">
        <ArrowLeft size={15} />
        <span className="brand-project">{projectName}</span>
      </button>
      {onCollapse && (
        <button
          className="icon-btn"
          title="Hide sidebar"
          aria-label="Hide sidebar"
          onClick={onCollapse}
        >
          <PanelLeft size={15} />
        </button>
      )}
    </div>
  );
}
