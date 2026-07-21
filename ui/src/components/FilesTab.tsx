import {
  Check,
  ChevronRight,
  Copy,
  ExternalLink,
  Code,
  FileText,
  FolderOpen,
  MousePointerClick,
  Settings2,
  Trash2,
} from "lucide-react";
import { useEffect, useRef, useState, type ReactNode } from "react";
import ReactMarkdown from "react-markdown";
import rehypeKatex from "rehype-katex";
import remarkGfm from "remark-gfm";
import remarkMath from "remark-math";
import {
  deleteFile,
  fileUrl,
  fmtBytes,
  getFileReport,
  type FileEntry,
  type Project,
  type ProjectFiles,
} from "../api";
import { CodeView } from "./CodeView";
import { mdCodeComponents, normalizeMathDelimiters } from "./Md";

/** Top-level folder reserved for project-wide reports (mirrors the backend). */
const PROJECT_NAMESPACE = "project";

/** Any href with a URI scheme (https:, mailto:, data:, …) or a
 * protocol-relative // — i.e. not a report-relative path to resolve. */
function isExternalSrc(src: string): boolean {
  return /^[a-z][a-z0-9+.-]*:/i.test(src) || src.startsWith("//");
}

/** Drop a leading YAML frontmatter block so it doesn't render as markdown. */
function stripFrontmatter(md: string): string {
  if (!md.startsWith("---")) return md;
  const end = md.indexOf("\n---", 3);
  return end === -1 ? md : md.slice(end + 4).replace(/^\r?\n/, "");
}

const IMAGE_RE = /\.(png|jpe?g|gif|webp|svg)$/i;
const MD_RE = /\.(md|mdx|markdown)$/i;
/** Raw text preview cap — matches the repo file viewer's truncation cap. */
const MAX_TEXT_PREVIEW = 512 * 1024;

/** Tree pane width: draggable divider, persisted across reloads. */
const TREE_WIDTH_KEY = "orx:files-tree-width";
const TREE_MIN_WIDTH = 180;
const TREE_MAX_WIDTH = 560;
const TREE_DEFAULT_WIDTH = 280;

function initialTreeWidth(): number {
  try {
    const w = Number(localStorage.getItem(TREE_WIDTH_KEY));
    if (Number.isFinite(w) && w >= TREE_MIN_WIDTH && w <= TREE_MAX_WIDTH) return w;
  } catch {
    // storage unavailable — fall through to the default
  }
  return TREE_DEFAULT_WIDTH;
}

/** Depth-first lookup of a tree entry by its dir-relative path. */
function findEntry(entries: FileEntry[], path: string): FileEntry | null {
  for (const e of entries) {
    if (e.path === path) return e;
    if (e.isDir && path.startsWith(e.path + "/")) {
      const hit = findEntry(e.children ?? [], path);
      if (hit) return hit;
    }
  }
  return null;
}

/** Every report folder in the tree, for the initial auto-selection. */
function collectReports(entries: FileEntry[]): FileEntry[] {
  const out: FileEntry[] = [];
  for (const e of entries) {
    if (!e.isDir) continue;
    if (e.reportTitle !== undefined) out.push(e);
    out.push(...collectReports(e.children ?? []));
  }
  return out;
}

/** Report markdown with report-relative image/link paths (`images/...`)
 * rewritten to the file endpoint, scoped to the report's folder. */
function ReportMd({
  projectId,
  folder,
  markdown,
}: {
  projectId: string;
  folder: string;
  markdown: string;
}) {
  const resolve = (src: string) => {
    if (isExternalSrc(src)) return src;
    const rel = src.replace(/^\.?\//, "");
    return fileUrl(projectId, folder ? `${folder}/${rel}` : rel);
  };
  return (
    <div className="md report-md">
      <ReactMarkdown
        remarkPlugins={[remarkGfm, remarkMath]}
        rehypePlugins={[rehypeKatex]}
        components={{
          // In-page anchors (headings, GFM footnotes) keep their hash href
          // and stay in the page; everything else resolves + opens a tab.
          a: ({ href, children, ...rest }) => {
            const isHash = !href || href.startsWith("#");
            return (
              <a
                {...rest}
                href={isHash ? href : resolve(href)}
                {...(isHash ? {} : { target: "_blank", rel: "noopener noreferrer" })}
              >
                {children}
              </a>
            );
          },
          img: ({ src, alt }) => {
            if (!src || typeof src !== "string") return null;
            const url = resolve(src);
            return (
              <a href={url} target="_blank" rel="noopener noreferrer" className="report-img">
                <img src={url} alt={alt ?? ""} loading="lazy" />
                {alt && <span className="report-img-caption">{alt}</span>}
              </a>
            );
          },
          ...mdCodeComponents,
        }}
      >
        {normalizeMathDelimiters(stripFrontmatter(markdown))}
      </ReactMarkdown>
    </div>
  );
}

type PreviewKind = "report" | "markdown" | "image" | "pdf" | "text";

function previewKind(entry: FileEntry): PreviewKind {
  // Only report folders are selectable (plain dirs merely toggle open), so
  // a dir here always has a report.md to render.
  if (entry.isDir) return "report";
  if (MD_RE.test(entry.name)) return "markdown";
  if (IMAGE_RE.test(entry.name)) return "image";
  if (/\.pdf$/i.test(entry.name)) return "pdf";
  return "text";
}

/** Fetched body for kinds that need text: report md, file md, or raw text.
 * `binary` flags NUL bytes so we don't dump garbage into a <pre>. */
function useTextBody(projectId: string, entry: FileEntry, kind: PreviewKind) {
  const [text, setText] = useState<string | null>(null);
  const [binary, setBinary] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const wantsText =
    kind === "report" || kind === "markdown" || (kind === "text" && entry.size <= MAX_TEXT_PREVIEW);

  useEffect(() => {
    // Reset before the wantsText guard: a refire on the same mounted entry
    // (modifiedAt changed — file rewritten on disk) must not leave the
    // previous body or binary/error flags behind.
    setText(null);
    setBinary(false);
    setError(null);
    if (!wantsText) return;
    let cancelled = false;
    const load =
      kind === "report"
        ? getFileReport(projectId, entry.path).then((r) => r.markdown)
        : fetch(fileUrl(projectId, entry.path)).then((r) => {
            if (!r.ok) throw new Error(`Failed to load file (${r.status})`);
            return r.text();
          });
    load
      .then((body) => {
        if (cancelled) return;
        if (body.includes("\u0000")) setBinary(true);
        else setText(body);
      })
      .catch((e) => {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e));
      });
    return () => {
      cancelled = true;
    };
  }, [projectId, entry.path, entry.modifiedAt, kind, wantsText]);

  return { text, binary, error, wantsText };
}

/** Right pane: the selected entry rendered inline — report folders and
 * markdown as documents, images/PDFs directly, everything else as code. */
function PreviewPane({
  projectId,
  entry,
  onDelete,
}: {
  projectId: string;
  entry: FileEntry;
  onDelete: (path: string) => void;
}) {
  const kind = previewKind(entry);
  const { text, binary, error, wantsText } = useTextBody(projectId, entry, kind);
  const [showSource, setShowSource] = useState(false);
  const isDoc = kind === "report" || kind === "markdown";
  // Reports resolve images relative to their folder; a bare .md file
  // resolves relative to its parent directory.
  const mdFolder = entry.isDir ? entry.path : entry.path.split("/").slice(0, -1).join("/");
  const rawUrl = fileUrl(projectId, entry.isDir ? `${entry.path}/report.md` : entry.path);

  let body: ReactNode;
  if (kind === "image") {
    body = (
      <a className="fpreview-image" href={rawUrl} target="_blank" rel="noopener noreferrer">
        <img src={rawUrl} alt={entry.name} />
      </a>
    );
  } else if (kind === "pdf") {
    body = <iframe className="fpreview-pdf" title={entry.name} src={rawUrl} />;
  } else if (!wantsText || binary) {
    body = (
      <div className="file-view-note">
        {binary ? "Binary file — no inline preview." : "File too large to preview inline."}{" "}
        <a href={rawUrl} target="_blank" rel="noopener noreferrer">
          Open raw
        </a>
      </div>
    );
  } else if (error) {
    body = <div className="file-view-note">Failed to load: {error}</div>;
  } else if (text === null) {
    body = (
      <div className="settings-loading">
        <span className="spinner" /> Loading…
      </div>
    );
  } else if (isDoc && !showSource) {
    body = <ReportMd projectId={projectId} folder={mdFolder} markdown={text} />;
  } else {
    body = <CodeView text={text} path={entry.isDir ? "report.md" : entry.path} />;
  }

  return (
    // `file-view` scopes the shared syntax-token colors onto the code view.
    <div className="fpreview file-view">
      <div className="fpreview-head">
        <FileText size={13} style={{ flexShrink: 0 }} />
        <code className="fpreview-path" title={entry.path}>
          {entry.isDir ? `${entry.path}/report.md` : entry.path}
        </code>
        <span className="fpreview-date">
          Modified{" "}
          {new Date(entry.modifiedAt).toLocaleString(undefined, {
            dateStyle: "medium",
            timeStyle: "short",
          })}
        </span>
        {!entry.isDir && kind === "text" && (
          <span className="fpreview-size">{fmtBytes(entry.size)}</span>
        )}
        {isDoc && (
          <button
            className={`icon-btn ${showSource ? "active" : ""}`}
            data-tip={showSource ? "Rendered view" : "View source"}
            aria-label={showSource ? "Rendered view" : "View source"}
            onClick={() => setShowSource((s) => !s)}
          >
            <Code size={13} />
          </button>
        )}
        <a
          className="icon-btn"
          href={rawUrl}
          target="_blank"
          rel="noopener noreferrer"
          data-tip="Open raw in new tab"
          aria-label="Open raw in new tab"
        >
          <ExternalLink size={13} />
        </a>
        <button
          className="icon-btn"
          data-tip={entry.isDir ? "Delete report folder" : "Delete file"}
          aria-label={entry.isDir ? "Delete report folder" : "Delete file"}
          onClick={() => {
            if (window.confirm(`Delete "${entry.path}" from the files dir?`))
              onDelete(entry.path);
          }}
        >
          <Trash2 size={13} />
        </button>
      </div>
      <div className={`fpreview-body ${isDoc && !showSource ? "doc" : ""}`}>{body}</div>
    </div>
  );
}

function TreeRows({
  projectId,
  entries,
  depth,
  collapsed,
  selected,
  onToggle,
  onSelect,
}: {
  projectId: string;
  entries: FileEntry[];
  depth: number;
  collapsed: Set<string>;
  selected: string | null;
  onToggle: (path: string) => void;
  onSelect: (path: string) => void;
}) {
  return (
    <>
      {entries.map((e) => {
        const indent = { paddingLeft: 8 + depth * 14 };
        if (e.isDir) {
          const isReport = e.reportTitle !== undefined;
          const open = !collapsed.has(e.path);
          // Report folders read as documents: the report's own title, with
          // the on-disk slug (and experiment status) in the tooltip.
          const label = isReport ? e.reportTitle : `${e.name}/`;
          const tooltip = isReport
            ? [e.name, e.experiment?.latestRunStatus].filter(Boolean).join(" — ")
            : undefined;
          return (
            <div key={e.path}>
              <div
                className={`ft-row ${selected === e.path ? "selected" : ""}`}
                style={indent}
                title={tooltip}
                onClick={() => (isReport ? onSelect(e.path) : onToggle(e.path))}
              >
                <button
                  className={`ft-chevron ${open ? "open" : ""}`}
                  aria-label={open ? `Collapse ${e.name}` : `Expand ${e.name}`}
                  onClick={(ev) => {
                    ev.stopPropagation();
                    onToggle(e.path);
                  }}
                >
                  <ChevronRight size={12} />
                </button>
                <span className={isReport ? "ft-title" : "ft-dirname"}>{label}</span>
              </div>
              {open && (e.children?.length ?? 0) > 0 && (
                <TreeRows
                  projectId={projectId}
                  entries={e.children ?? []}
                  depth={depth + 1}
                  collapsed={collapsed}
                  selected={selected}
                  onToggle={onToggle}
                  onSelect={onSelect}
                />
              )}
            </div>
          );
        }

        return (
          <div
            key={e.path}
            className={`ft-row file ${selected === e.path ? "selected" : ""}`}
            style={indent}
            title={e.path}
            onClick={() => onSelect(e.path)}
          >
            <span className="ft-chevron spacer" />
            {IMAGE_RE.test(e.name) && (
              <img className="ft-thumb" src={fileUrl(projectId, e.path)} alt="" loading="lazy" />
            )}
            <span className="ft-name">{e.name}</span>
          </div>
        );
      })}
    </>
  );
}

/** Top-level ordering that mirrors the layout convention: the reserved
 * `project/` namespace first, then experiment folders, then everything else
 * (which keeps its dirs-then-files explorer order). */
function groupTopLevel(entries: FileEntry[]): {
  project: FileEntry[];
  experiments: FileEntry[];
  other: FileEntry[];
} {
  const project: FileEntry[] = [];
  const experiments: FileEntry[] = [];
  const other: FileEntry[] = [];
  for (const e of entries) {
    if (e.isDir && e.name === PROJECT_NAMESPACE) project.push(e);
    else if (e.isDir && e.experiment) experiments.push(e);
    else other.push(e);
  }
  return { project, experiments, other };
}

/** The files dir path, copyable — demoted to a footer under the tree. */
function DirFooter({ dir, onOpenStorage }: { dir: string; onOpenStorage: () => void }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="ftree-footer" title={dir}>
      <code>{dir}</code>
      <button
        className="icon-btn tip-up"
        data-tip={copied ? "Copied!" : "Copy path"}
        aria-label="Copy files directory path"
        onClick={() => {
          void navigator.clipboard?.writeText(dir);
          setCopied(true);
          setTimeout(() => setCopied(false), 1200);
        }}
      >
        {copied ? <Check size={12} /> : <Copy size={12} />}
      </button>
      <button
        className="icon-btn tip-up"
        data-tip="Storage settings"
        aria-label="Storage settings"
        onClick={onOpenStorage}
      >
        <Settings2 size={12} />
      </button>
    </div>
  );
}

/** Middle-pane Files tab — a split explorer over the project's files folder
 * on disk. Tree on the left; the selected entry renders inline on the right
 * (reports and markdown as documents, images and PDFs directly, code as
 * highlighted source). Top-level folders correspond to experiments, with the
 * reserved `project/` namespace for project-wide reports pinned first. */
export function FilesTab({
  project,
  files,
  onChanged,
  onOpenStorage,
}: {
  project: Project;
  files: ProjectFiles | null;
  onChanged: () => void;
  /** Navigate to Settings → Storage (where the data dir can be changed). */
  onOpenStorage: () => void;
}) {
  const [selected, setSelected] = useState<string | null>(null);
  // Folders are open by default — including ones that appear later, when the
  // agent writes a new report — so this tracks what the user closed instead.
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const [autoSelected, setAutoSelected] = useState(false);
  const [treeWidth, setTreeWidth] = useState(initialTreeWidth);
  const treeRef = useRef<HTMLDivElement>(null);

  // Drag the divider to resize the tree pane; width persists across reloads.
  // Mirrors App's right-panel resizer: capture the pointer so views under the
  // cursor don't steal the drag, and suppress text selection while dragging.
  const resizeTree = (e: React.PointerEvent) => {
    e.preventDefault();
    e.currentTarget.setPointerCapture(e.pointerId);
    const left = treeRef.current?.getBoundingClientRect().left ?? 0;
    const prevUserSelect = document.body.style.userSelect;
    document.body.style.userSelect = "none";
    const onMove = (ev: PointerEvent) => {
      const w = Math.round(ev.clientX - left);
      const clamped = Math.min(Math.max(w, TREE_MIN_WIDTH), TREE_MAX_WIDTH);
      setTreeWidth(clamped);
      try {
        localStorage.setItem(TREE_WIDTH_KEY, String(clamped));
      } catch {
        // best-effort persistence
      }
    };
    const stop = () => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", stop);
      window.removeEventListener("pointercancel", stop);
      document.body.style.userSelect = prevUserSelect;
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", stop);
    window.addEventListener("pointercancel", stop);
  };

  // First load: open the most recent report so the pane isn't dead space.
  useEffect(() => {
    if (!files || autoSelected) return;
    setAutoSelected(true);
    if (selected) return;
    const reports = collectReports(files.entries);
    if (reports.length === 0) return;
    const latest = reports.reduce((a, b) => (b.modifiedAt > a.modifiedAt ? b : a));
    setSelected(latest.path);
  }, [files, autoSelected, selected]);

  // The selection vanished from disk (deleted externally) — clear it.
  useEffect(() => {
    if (selected && files && !findEntry(files.entries, selected)) setSelected(null);
  }, [selected, files]);

  const toggle = (path: string) =>
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });

  const remove = (path: string) => {
    if (selected === path || selected?.startsWith(path + "/")) setSelected(null);
    void deleteFile(project.id, path)
      .catch(() => {})
      .finally(onChanged);
  };

  if (!files) {
    return (
      <div className="files-tab">
        <div className="settings-loading" style={{ padding: 20 }}>
          <span className="spinner" /> Loading files…
        </div>
      </div>
    );
  }

  const { project: projectNs, experiments, other } = groupTopLevel(files.entries);
  const tree = (entries: FileEntry[]) => (
    <TreeRows
      projectId={project.id}
      entries={entries}
      depth={0}
      collapsed={collapsed}
      selected={selected}
      onToggle={toggle}
      onSelect={setSelected}
    />
  );
  const selectedEntry = selected ? findEntry(files.entries, selected) : null;

  if (files.entries.length === 0) {
    return (
      <div className="files-tab">
        <div className="files-empty-state">
          <FolderOpen size={28} strokeWidth={1.5} />
          <h3>No files yet</h3>
          <p>
            This is where the agent saves the project's files — experiment reports, figures, and
            other artifacts. Ask it for a write-up of its findings and the report will land
            here. You can also drop your own files into the folder:
          </p>
          <DirFooter dir={files.dir} onOpenStorage={onOpenStorage} />
        </div>
      </div>
    );
  }

  return (
    <div className="files-tab">
      <div className="ftree-pane" ref={treeRef} style={{ width: treeWidth }}>
        <div className="ftree-resizer" onPointerDown={resizeTree} />
        <div className="ftree-scroll">
          {tree(projectNs)}
          {tree(experiments)}
          {other.length > 0 && (experiments.length > 0 || projectNs.length > 0) && (
            <div className="ft-divider">Other files</div>
          )}
          {tree(other)}
          {files.truncated && (
            <p className="files-truncated">Listing truncated — the folder has more files.</p>
          )}
        </div>
        <DirFooter dir={files.dir} onOpenStorage={onOpenStorage} />
      </div>
      {selectedEntry ? (
        // Keyed by path so per-file view state (source toggle, fetched body)
        // starts fresh on every selection instead of leaking across files.
        <PreviewPane
          key={selectedEntry.path}
          projectId={project.id}
          entry={selectedEntry}
          onDelete={remove}
        />
      ) : (
        <div className="fpreview fpreview-none">
          <MousePointerClick size={22} strokeWidth={1.5} />
          <span>Click a file to view it</span>
        </div>
      )}
    </div>
  );
}
