// The project's code browser: the committed tree of an experiment branch, or
// the hub clone's checkout, picked from a header select (opened from an
// experiment card's Code shortcut). Source + expansion state live on the
// App-side tab def (the component unmounts whenever another right-pane tab
// fronts it). The tree is built from git listings (gitignored trees
// excluded). Clicking a file opens the existing FileViewer tab, served from
// the same source.

import {
  ChevronDown,
  ChevronRight,
  File as FileIcon,
  Folder,
  FolderOpen,
  RotateCw,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  getCodeTree,
  githubBranchUrl,
  type CodeTree,
  type Experiment,
  type Project,
} from "../api";
import { GitHubMark } from "./BackendLogos";

/** A node in the nested tree derived from the flat path list. */
interface DirNode {
  /** Child directories, keyed by name, sorted on render. */
  dirs: Map<string, DirNode>;
  /** File names directly in this dir. */
  files: string[];
}

function emptyDir(): DirNode {
  return { dirs: new Map(), files: [] };
}

/** Build a nested dir tree from sorted repo-relative paths. */
function buildTree(entries: string[]): DirNode {
  const root = emptyDir();
  for (const path of entries) {
    const parts = path.split("/");
    let node = root;
    for (let i = 0; i < parts.length - 1; i++) {
      const name = parts[i];
      let next = node.dirs.get(name);
      if (!next) {
        next = emptyDir();
        node.dirs.set(name, next);
      }
      node = next;
    }
    node.files.push(parts[parts.length - 1]);
  }
  return root;
}

// Open/closed is a depth rule plus a set of user exceptions: top-level dirs
// default open, deeper ones default closed, and a toggle flips a dir away
// from its default. No seeding pass — dirs appearing in later refreshes
// behave exactly like their siblings.

function DirRow({
  name,
  node,
  path,
  depth,
  toggled,
  onToggle,
  onOpenFile,
}: {
  name: string;
  node: DirNode;
  /** Repo-relative dir path (toggle-state key). */
  path: string;
  depth: number;
  toggled: ReadonlySet<string>;
  onToggle: (path: string) => void;
  onOpenFile: (path: string) => void;
}) {
  const defaultOpen = depth === 0;
  const isOpen = toggled.has(path) ? !defaultOpen : defaultOpen;
  return (
    <>
      <button
        type="button"
        className="code-tree-row"
        style={{ paddingLeft: 8 + depth * 14 }}
        onClick={() => onToggle(path)}
        title={path}
      >
        {isOpen ? (
          <ChevronDown size={13} className="code-tree-chev" />
        ) : (
          <ChevronRight size={13} className="code-tree-chev" />
        )}
        {isOpen ? <FolderOpen size={13} /> : <Folder size={13} />}
        <span className="code-tree-name">{name}</span>
      </button>
      {isOpen && (
        <TreeLevel
          node={node}
          parentPath={path}
          depth={depth + 1}
          toggled={toggled}
          onToggle={onToggle}
          onOpenFile={onOpenFile}
        />
      )}
    </>
  );
}

function TreeLevel({
  node,
  parentPath,
  depth,
  toggled,
  onToggle,
  onOpenFile,
}: {
  node: DirNode;
  parentPath: string;
  depth: number;
  toggled: ReadonlySet<string>;
  onToggle: (path: string) => void;
  onOpenFile: (path: string) => void;
}) {
  const dirNames = [...node.dirs.keys()].sort((a, b) => a.localeCompare(b));
  const fileNames = [...node.files].sort((a, b) => a.localeCompare(b));
  return (
    <>
      {dirNames.map((name) => {
        const path = parentPath ? `${parentPath}/${name}` : name;
        return (
          <DirRow
            key={`d:${path}`}
            name={name}
            node={node.dirs.get(name)!}
            path={path}
            depth={depth}
            toggled={toggled}
            onToggle={onToggle}
            onOpenFile={onOpenFile}
          />
        );
      })}
      {fileNames.map((name) => {
        const path = parentPath ? `${parentPath}/${name}` : name;
        return (
          <button
            key={`f:${path}`}
            type="button"
            className="code-tree-row"
            style={{ paddingLeft: 8 + depth * 14 }}
            onClick={() => onOpenFile(path)}
            title={path}
          >
            <FileIcon size={13} />
            <span className="code-tree-name">{name}</span>
          </button>
        );
      })}
    </>
  );
}

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
