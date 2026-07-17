// A code browser for one chat session: either the live session worktree or
// the committed tree of an experiment branch, picked from a header select.
// Source + expansion state live on the App-side tab def (the component
// unmounts whenever another right-pane tab fronts it). The tree is built
// from git listings (gitignored trees excluded; the live view also shows
// untracked new files). Clicking a file opens the existing FileViewer tab,
// served from the same source.

import {
  ChevronDown,
  ChevronRight,
  ExternalLink,
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
// from its default. No seeding pass — dirs appearing in later polls behave
// exactly like their siblings.

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
        className="code-tree-row code-tree-dir"
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
            className="code-tree-row code-tree-file"
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
  sessionId,
  project,
  experiments,
  sel,
  selPicked,
  toggled,
  onSelChange,
  onToggledChange,
  onOpenFile,
}: {
  projectId: string;
  /** Chat session whose worktree the live view browses. */
  sessionId: string;
  /** Owning project — supplies owner/repo for the GitHub branch link. */
  project: Project;
  /** Project experiments — one selectable branch entry each (deduped). */
  experiments: Experiment[];
  /** Source to browse: "" = live session worktree, else a branch name.
   * Lives on the tab def so it survives unmount/remount. */
  sel: string;
  /** Whether `sel` was picked by the user (defaults may fall back to live
   * when their branch doesn't exist yet; user picks never do). */
  selPicked: boolean;
  /** Dirs flipped away from their depth default (lives on the tab def). */
  toggled: ReadonlySet<string>;
  onSelChange: (sel: string, picked: boolean) => void;
  onToggledChange: (toggled: ReadonlySet<string>) => void;
  /** Open a file in the right pane's FileViewer, keyed to this source. */
  onOpenFile: (path: string, sessionId?: string, ref?: string) => void;
}) {
  const [data, setData] = useState<CodeTree | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  // A request id drops stale responses — from earlier sources, superseded
  // polls, and (via the effect-cleanup bump) post-unmount completions. The
  // in-flight flag only throttles the background poll.
  const reqId = useRef(0);
  const inFlight = useRef(false);
  // The fallback callback/flag ride refs so `load`'s identity — and with it
  // the fetch effect — only tracks real source changes.
  const onSelChangeRef = useRef(onSelChange);
  onSelChangeRef.current = onSelChange;
  const selPickedRef = useRef(selPicked);
  selPickedRef.current = selPicked;

  const load = useCallback(
    (showSpinner: boolean) => {
      const id = ++reqId.current;
      inFlight.current = true;
      if (showSpinner) setLoading(true);
      getCodeTree(projectId, sel ? { ref: sel } : { sessionId })
        .then((d) => {
          if (id !== reqId.current) return;
          setData(d);
          setError(null);
        })
        .catch((e: Error) => {
          if (id !== reqId.current) return;
          // A seeded default can name a branch that doesn't exist yet (run
          // just started) — fall back to live instead of erroring. A branch
          // the user picked keeps the error.
          if (sel && !selPickedRef.current && e.message === "branch not found") {
            onSelChangeRef.current("", false);
            return;
          }
          // Spinner loads (initial fetch, manual Refresh) surface errors;
          // background polls swallow them — transient, the next tick retries.
          if (showSpinner) setError(e.message);
        })
        .finally(() => {
          if (id === reqId.current) {
            inFlight.current = false;
            setLoading(false);
          }
        });
    },
    [projectId, sessionId, sel],
  );

  // Fetch on mount and whenever the source changes; a stale tree from the
  // previous source must not linger under the new header. The cleanup bump
  // invalidates in-flight responses on source change and unmount.
  useEffect(() => {
    setData(null);
    setError(null);
    load(true);
    return () => {
      reqId.current++;
    };
  }, [load]);

  // Poll only the live view (the agent writes files as it works); a branch's
  // committed tree changes on commit — the manual Refresh covers that.
  useEffect(() => {
    if (sel) return;
    const timer = setInterval(() => {
      if (!inFlight.current) load(false);
    }, 5000);
    return () => clearInterval(timer);
  }, [sel, load]);

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

  // GitHub link target: the picked branch, or whatever the live view has
  // checked out (none while detached).
  const linkBranch = sel || data?.branch || null;

  return (
    <div className="code-tab">
      <div className="code-tab-header">
        <select
          className="input sm code-tab-select"
          value={sel}
          onChange={(e) => onSelChange(e.target.value, true)}
          title="Source to browse"
        >
          <option value="">live — session worktree</option>
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
            <ExternalLink size={13} />
          </a>
        )}
        <span style={{ flex: 1 }} />
        <button
          className="icon-btn"
          title="Refresh"
          aria-label="Refresh"
          onClick={() => load(true)}
        >
          {loading ? <span className="spinner" /> : <RotateCw size={13} />}
        </button>
      </div>
      {!sel && data?.root === "clone" && (
        <div className="code-tab-note">
          session worktree unavailable — showing the project clone
        </div>
      )}
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
              onOpenFile={(path) => onOpenFile(path, sessionId, sel || undefined)}
            />
          </div>
        )}
      </div>
    </div>
  );
}
