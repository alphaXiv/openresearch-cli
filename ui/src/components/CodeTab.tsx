// A code browser for one chat session: either the live session worktree or
// the committed tree of an experiment branch, picked from a header select.
// The tree is built from git listings (gitignored trees excluded; the live
// view also shows untracked new files). Clicking a file opens the existing
// FileViewer tab, served from the same source.

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
  type Run,
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

/** The branch of the experiment owning the project's most recent run — what
 * the user most likely wants to browse — or "" (live worktree) without runs. */
function defaultSelection(experiments: Experiment[], runs: Run[]): string {
  if (runs.length === 0) return "";
  const latest = runs.reduce((a, b) => (b.createdAt > a.createdAt ? b : a));
  return experiments.find((e) => e.id === latest.experimentId)?.branchName ?? "";
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
  toggled: Set<string>;
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
  toggled: Set<string>;
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
  runs,
  onOpenFile,
}: {
  projectId: string;
  /** Chat session whose worktree the live view browses. */
  sessionId: string;
  /** Owning project — supplies owner/repo for the GitHub branch link. */
  project: Project;
  /** Project experiments — one selectable branch entry each (deduped). */
  experiments: Experiment[];
  /** Project runs — pick the default branch (latest run's experiment). */
  runs: Run[];
  /** Open a file in the right pane's FileViewer, keyed to this source. */
  onOpenFile: (path: string, sessionId?: string, ref?: string) => void;
}) {
  const [data, setData] = useState<CodeTree | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  // "" = the live session worktree; anything else = a branch name whose
  // committed tree is browsed. Defaults to the latest-run experiment's branch.
  const [sel, setSel] = useState(() => defaultSelection(experiments, runs));
  // Dirs the user flipped away from their depth default (see DirRow).
  const [toggled, setToggled] = useState<Set<string>>(new Set());
  // Unmount guard (FileViewer's cancelled-flag pattern); a request id keeps
  // stale responses from clobbering a newer selection; the in-flight flag
  // only throttles the background poll (user actions always fetch).
  const mounted = useRef(true);
  const reqId = useRef(0);
  const inFlight = useRef(false);
  const dataRef = useRef<CodeTree | null>(null);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  const load = useCallback(
    (showSpinner: boolean) => {
      const id = ++reqId.current;
      inFlight.current = true;
      if (showSpinner) setLoading(true);
      getCodeTree(projectId, sel ? { ref: sel } : { sessionId })
        .then((d) => {
          if (!mounted.current || id !== reqId.current) return;
          dataRef.current = d;
          setData(d);
          setError(null);
        })
        .catch((e: Error) => {
          // Background poll failures are transient (the next tick retries) —
          // keep rendering the last-good tree; only surface an error when
          // there's nothing to show yet.
          if (mounted.current && id === reqId.current && !dataRef.current) setError(e.message);
        })
        .finally(() => {
          inFlight.current = false;
          if (mounted.current && id === reqId.current) setLoading(false);
        });
    },
    [projectId, sessionId, sel],
  );

  // Fetch on mount and whenever the source changes; a stale tree from the
  // previous source must not linger under the new header.
  useEffect(() => {
    dataRef.current = null;
    setData(null);
    setError(null);
    load(true);
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

  const toggle = useCallback((path: string) => {
    setToggled((prev) => {
      const next = new Set(prev);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
  }, []);

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
          onChange={(e) => setSel(e.target.value)}
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
        <div className="code-tab-note">session worktree unavailable — showing the hub clone</div>
      )}
      {data?.truncated && <div className="code-tab-note">listing truncated</div>}
      <div className="code-tab-body">
        {error ? (
          <div className="code-tab-note">Failed to load: {error}</div>
        ) : !tree ? (
          <div className="code-tab-note">Loading…</div>
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
