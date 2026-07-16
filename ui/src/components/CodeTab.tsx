// A code browser over one chat session's worktree: a collapsible file tree
// built from `git ls-files` (so gitignored trees are excluded and untracked
// new files are included), plus a GitHub branch link. Clicking a file opens
// the existing FileViewer tab, served from the same session worktree.

import {
  ChevronDown,
  ChevronRight,
  File as FileIcon,
  Folder,
  FolderOpen,
  RotateCw,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { getCodeTree, type CodeTree, type Project } from "../api";
import { BranchPill } from "./BranchPill";

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
  onOpenFile,
}: {
  projectId: string;
  /** Chat session whose worktree this browses. */
  sessionId: string;
  /** Owning project — supplies owner/repo for the GitHub branch link. */
  project: Project;
  /** Open a file in the right pane's FileViewer, keyed to this session. */
  onOpenFile: (path: string, sessionId?: string) => void;
}) {
  const [data, setData] = useState<CodeTree | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  // Dirs the user flipped away from their depth default (see DirRow).
  const [toggled, setToggled] = useState<Set<string>>(new Set());
  // Unmount guard (FileViewer's cancelled-flag pattern) + in-flight guard so
  // a slow response doesn't stack polls.
  const mounted = useRef(true);
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
      if (inFlight.current) return;
      inFlight.current = true;
      if (showSpinner) setLoading(true);
      getCodeTree(projectId, sessionId)
        .then((d) => {
          if (!mounted.current) return;
          dataRef.current = d;
          setData(d);
          setError(null);
        })
        .catch((e: Error) => {
          // Background poll failures are transient (the next tick retries) —
          // keep rendering the last-good tree; only surface an error when
          // there's nothing to show yet.
          if (mounted.current && !dataRef.current) setError(e.message);
        })
        .finally(() => {
          inFlight.current = false;
          if (mounted.current) setLoading(false);
        });
    },
    [projectId, sessionId],
  );

  // Load on mount / session change.
  useEffect(() => {
    load(true);
  }, [load]);

  // Poll while visible so files the agent just wrote show up.
  useEffect(() => {
    const timer = setInterval(() => load(false), 5000);
    return () => clearInterval(timer);
  }, [load]);

  const tree = useMemo(() => (data ? buildTree(data.entries) : null), [data]);

  const toggle = useCallback((path: string) => {
    setToggled((prev) => {
      const next = new Set(prev);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
  }, []);

  return (
    <div className="code-tab">
      <div className="code-tab-header">
        {data?.branch ? (
          <BranchPill owner={project.githubOwner} repo={project.githubRepo} branch={data.branch} />
        ) : (
          <span className="code-tab-branch-none">{data ? "no branch checked out" : "…"}</span>
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
      {data?.root === "clone" && (
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
              onOpenFile={(path) => onOpenFile(path, sessionId)}
            />
          </div>
        )}
      </div>
    </div>
  );
}
