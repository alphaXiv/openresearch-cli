// The project's code browser: the committed tree of an experiment branch, or
// the hub clone's checkout, picked from a header select (opened from an
// experiment card's Code shortcut). Source + expansion state live on the
// App-side tab def (the component unmounts whenever another right-pane tab
// fronts it). The tree is built from git listings (gitignored trees
// excluded). Clicking a file opens the existing FileViewer tab, served from
// the same source.

import { RotateCw } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  getCodeTree,
  githubBranchUrl,
  type CodeTree,
  type Experiment,
  type Project,
} from "../api";
import { GitHubMark } from "./BackendLogos";
import { buildTree, TreeLevel } from "./codeTree";

export function CodeTab({
  projectId,
  project,
  experiments,
  sel,
  toggled,
  onSelChange,
  onToggledChange,
  onOpenFile,
}: {
  projectId: string;
  /** Owning project — supplies owner/repo for the GitHub branch link. */
  project: Project;
  /** Project experiments — one selectable branch entry each (deduped). */
  experiments: Experiment[];
  /** Source to browse: "" = the project clone, else a branch name. Lives on
   * the tab def so it survives unmount/remount. */
  sel: string;
  /** Dirs flipped away from their depth default (lives on the tab def). */
  toggled: ReadonlySet<string>;
  onSelChange: (sel: string) => void;
  onToggledChange: (toggled: ReadonlySet<string>) => void;
  /** Open a file in the right pane's FileViewer, keyed to this source. */
  onOpenFile: (path: string, sessionId?: string, ref?: string) => void;
}) {
  const [data, setData] = useState<CodeTree | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  // A request id drops stale responses — from earlier sources, superseded
  // refreshes, and (via the effect-cleanup bump) post-unmount completions.
  const reqId = useRef(0);

  const load = useCallback(() => {
    const id = ++reqId.current;
    setLoading(true);
    getCodeTree(projectId, sel ? { ref: sel } : {})
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
  }, [projectId, sel]);

  // Fetch on mount and whenever the source changes (plus manual Refresh —
  // committed trees only move on commit, so there's no poll); a stale tree
  // from the previous source must not linger under the new header. The
  // cleanup bump invalidates in-flight responses on source change and
  // unmount.
  useEffect(() => {
    setData(null);
    setError(null);
    load();
    return () => {
      reqId.current++;
    };
  }, [load]);

  const tree = useMemo(() => (data ? buildTree(data.entries) : null), [data]);

  const toggle = useCallback(
    (path: string) => {
      const next = new Set(toggled);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      onToggledChange(next);
    },
    [toggled, onToggledChange],
  );

  // One entry per branch: several experiments (e.g. baselines) can share one.
  const branchOptions = useMemo(() => {
    const seen = new Set<string>();
    const options: { branch: string; label: string }[] = [];
    for (const e of experiments) {
      if (seen.has(e.branchName)) continue;
      seen.add(e.branchName);
      options.push({ branch: e.branchName, label: e.slug });
    }
    return options;
  }, [experiments]);

  // GitHub link target: the picked branch, or whatever the clone has
  // checked out (none while detached).
  const linkBranch = sel || data?.branch || null;

  return (
    <div className="code-tab">
      <div className="code-tab-header">
        <span className="ctl-label">Source</span>
        <select
          className="input sm code-tab-select"
          value={sel}
          onChange={(e) => onSelChange(e.target.value)}
          title="Source to browse"
        >
          <option value="">project clone</option>
          {/* A pinned branch can drop out of the options (its experiment's
              branchName changed under us) — keep the select truthful. */}
          {sel && !branchOptions.some((o) => o.branch === sel) && (
            <option value={sel}>{sel}</option>
          )}
          {branchOptions.map((o) => (
            <option key={o.branch} value={o.branch}>
              {o.label}
            </option>
          ))}
        </select>
        {linkBranch && (
          <a
            className="icon-btn"
            href={githubBranchUrl(project.githubOwner, project.githubRepo, linkBranch)}
            target="_blank"
            rel="noopener noreferrer"
            title={`Open ${linkBranch} on GitHub`}
          >
            <GitHubMark size={13} />
          </a>
        )}
        <span style={{ flex: 1 }} />
        <button
          className="icon-btn"
          title="Refresh"
          aria-label="Refresh"
          onClick={load}
        >
          {loading ? <span className="spinner" /> : <RotateCw size={13} />}
        </button>
      </div>
      {data?.truncated && <div className="code-tab-note">listing truncated</div>}
      {error && tree && <div className="code-tab-note">Refresh failed: {error}</div>}
      <div className="code-tab-body">
        {!tree ? (
          <div className="code-tab-note">{error ? `Failed to load: ${error}` : "Loading…"}</div>
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
              onOpenFile={(path) => onOpenFile(path, undefined, sel || undefined)}
            />
          </div>
        )}
      </div>
    </div>
  );
}
