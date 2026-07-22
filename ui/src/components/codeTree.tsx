// The nested file-tree primitives shared by the code browsers: the committed
// CodeTab and the live WorktreeTab's Files view. A flat, sorted, repo-relative
// path list (from a git listing) becomes a nested dir tree that renders as
// collapsible rows; clicking a file bubbles its repo-relative path up. Kept
// source-agnostic — the caller decides which checkout the paths came from and
// how a file open resolves.

import { ChevronDown, ChevronRight, File as FileIcon, Folder, FolderOpen } from "lucide-react";

/** A node in the nested tree derived from the flat path list. */
export interface DirNode {
  /** Child directories, keyed by name, sorted on render. */
  dirs: Map<string, DirNode>;
  /** File names directly in this dir. */
  files: string[];
}

function emptyDir(): DirNode {
  return { dirs: new Map(), files: [] };
}

/** Build a nested dir tree from sorted repo-relative paths. */
export function buildTree(entries: string[]): DirNode {
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

export function TreeLevel({
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
