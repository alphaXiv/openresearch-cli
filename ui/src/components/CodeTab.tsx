// Files and committed changes for one experiment branch. The opening
// experiment fixes the Git source; users only switch between Files/Changes.

import { GitBranch, RotateCw } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { getCodeTree, type CodeTree, type Experiment } from "../api";
import { BranchChanges } from "./BranchChanges";
import { buildTree, TreeLevel } from "./codeTree";

export type CodeView = "files" | "changes";

export function CodeTab({
  projectId,
  experiment,
  view,
  toggled,
  onViewChange,
  onToggledChange,
  onOpenFile,
}: {
  projectId: string;
  /** Experiment whose committed Git branch this tab displays. */
  experiment: Experiment;
  view: CodeView;
  /** Dirs flipped away from their depth default (lives on the tab def). */
  toggled: ReadonlySet<string>;
  onViewChange: (view: CodeView) => void;
  onToggledChange: (toggled: ReadonlySet<string>) => void;
  /** Open a file in the right pane's FileViewer, keyed to this source. */
  onOpenFile: (path: string, sessionId?: string, ref?: string) => void;
}) {
  const branch = experiment.branchName;
  const sourceKey = `${projectId}:${branch}`;
  const [data, setData] = useState<CodeTree | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [changesLoading, setChangesLoading] = useState(false);
  const [changesRefreshKey, setChangesRefreshKey] = useState(0);
  // A request id drops stale responses — from earlier sources, superseded
  // refreshes, and (via the effect-cleanup bump) post-unmount completions.
  const reqId = useRef(0);
  const requestedSource = useRef<string | null>(null);

  const load = useCallback(() => {
    requestedSource.current = sourceKey;
    const id = ++reqId.current;
    setLoading(true);
    getCodeTree(projectId, { ref: branch })
      .then((d) => {
        if (id !== reqId.current) return;
        setData(d);
        setError(null);
      })
      .catch((e: Error) => {
        if (id !== reqId.current) return;
        setError(e.message);
      })
      .finally(() => {
        if (id === reqId.current) setLoading(false);
      });
  }, [projectId, branch, sourceKey]);

  // Clear a previous branch's tree immediately and invalidate its requests.
  useEffect(() => {
    reqId.current++;
    requestedSource.current = null;
    setData(null);
    setError(null);
    setLoading(false);
    return () => {
      reqId.current++;
    };
  }, [sourceKey]);

  // Changes can open without paying for an unused tree request. Load the tree
  // once when Files is first shown; manual Refresh can still call load again.
  useEffect(() => {
    if (view === "files" && requestedSource.current !== sourceKey) load();
  }, [view, sourceKey, load]);

  const tree = useMemo(() => (data ? buildTree(data.entries) : null), [data]);
  const refreshing = view === "files" ? loading : changesLoading;

  const toggle = useCallback(
    (path: string) => {
      const next = new Set(toggled);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      onToggledChange(next);
    },
    [toggled, onToggledChange],
  );

  return (
    <div className="code-tab">
      <div className="code-tab-header">
        <div className="seg">
          <button className={view === "files" ? "active" : ""} onClick={() => onViewChange("files")}>
            Files
          </button>
          <button className={view === "changes" ? "active" : ""} onClick={() => onViewChange("changes")}>
            Changes
          </button>
        </div>
        <span className="wt-branch-chip" title={`Committed branch ${branch}`}>
          <GitBranch size={12} />
          <span className="wt-branch-name">{branch}</span>
        </span>
        <span style={{ flex: 1 }} />
        <button
          className="icon-btn"
          title="Refresh"
          aria-label="Refresh"
          onClick={() =>
            view === "files" ? load() : setChangesRefreshKey((current) => current + 1)
          }
        >
          {refreshing ? <span className="spinner" /> : <RotateCw size={13} />}
        </button>
      </div>
      {view === "changes" ? (
        <BranchChanges
          key={experiment.id}
          experiment={experiment}
          refreshKey={changesRefreshKey}
          onLoadingChange={setChangesLoading}
        />
      ) : (
        <>
          {data?.truncated && <div className="code-tab-note">listing truncated</div>}
          {error && tree && <div className="code-tab-note">Refresh failed: {error}</div>}
          <div className="code-tab-body">
            {!tree ? (
              <div className="code-tab-note">
                {error ? `Failed to load: ${error}` : "Loading…"}
              </div>
            ) : tree.dirs.size === 0 && tree.files.length === 0 ? (
              <div className="code-tab-note">No files.</div>
            ) : (
              <div className="code-tree">
                <TreeLevel
                  node={tree}
                  parentPath=""
                  depth={0}
                  toggled={toggled}
                  onToggle={toggle}
                  onOpenFile={(path) => onOpenFile(path, undefined, branch)}
                />
              </div>
            )}
          </div>
        </>
      )}
    </div>
  );
}
