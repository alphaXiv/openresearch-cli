import { ArrowLeft, Check, ChevronRight, Copy, ExternalLink, Trash2 } from "lucide-react";
import { useEffect, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import {
  deleteFile,
  fileUrl,
  getFileReport,
  type FileEntry,
  type Project,
  type ProjectFiles,
} from "../api";

/** Top-level folder reserved for project-wide reports (mirrors the backend). */
const PROJECT_NAMESPACE = "project";

function isExternalSrc(src: string): boolean {
  return /^(https?:)?\/\//i.test(src) || src.startsWith("data:");
}

/** Drop a leading YAML frontmatter block so it doesn't render as markdown. */
function stripFrontmatter(md: string): string {
  if (!md.startsWith("---")) return md;
  const end = md.indexOf("\n---", 3);
  return end === -1 ? md : md.slice(end + 4).replace(/^\r?\n/, "");
}

function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

const IMAGE_RE = /\.(png|jpe?g|gif|webp|svg)$/i;

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
  const resolve = (src: string) =>
    isExternalSrc(src) ? src : fileUrl(projectId, `${folder}/${src.replace(/^\.?\//, "")}`);
  return (
    <div className="md report-md">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        components={{
          a: ({ href, children, ...rest }) => (
            <a
              {...rest}
              href={href && !href.startsWith("#") ? resolve(href) : href}
              target="_blank"
              rel="noopener noreferrer"
            >
              {children}
            </a>
          ),
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
        }}
      >
        {stripFrontmatter(markdown)}
      </ReactMarkdown>
    </div>
  );
}

function ReportView({
  projectId,
  entry,
  onBack,
  onDelete,
}: {
  projectId: string;
  entry: FileEntry;
  onBack: () => void;
  onDelete: () => void;
}) {
  const [markdown, setMarkdown] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    getFileReport(projectId, entry.path)
      .then((r) => setMarkdown(r.markdown))
      .catch((e) => setError(e instanceof Error ? e.message : String(e)));
  }, [projectId, entry.path, entry.modifiedAt]);

  return (
    <div className="report-view">
      <div className="report-view-col">
        <div className="report-view-head">
          <button className="report-back" onClick={onBack}>
            <ArrowLeft size={13} /> Files
          </button>
          <span style={{ flex: 1 }} />
          <span className="report-date">{new Date(entry.modifiedAt).toLocaleString()}</span>
          <button
            className="icon-btn"
            title="Delete report folder"
            aria-label="Delete report folder"
            onClick={() => {
              if (window.confirm(`Delete the "${entry.path}" folder from the files dir?`))
                onDelete();
            }}
          >
            <Trash2 size={14} />
          </button>
        </div>
        {error ? (
          <div className="error">{error}</div>
        ) : markdown === null ? (
          <div className="settings-loading">
            <span className="spinner" /> Loading report…
          </div>
        ) : (
          <ReportMd projectId={projectId} folder={entry.path} markdown={markdown} />
        )}
      </div>
    </div>
  );
}

/** The files dir path, copyable — where the user (or agent) drops files. */
function DirPath({ dir }: { dir: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="files-pill files-pill-dir" title={dir}>
      <code>{dir}</code>
      <button
        className="icon-btn"
        title="Copy path"
        aria-label="Copy files directory path"
        onClick={() => {
          void navigator.clipboard?.writeText(dir);
          setCopied(true);
          setTimeout(() => setCopied(false), 1200);
        }}
      >
        {copied ? <Check size={13} /> : <Copy size={13} />}
      </button>
    </div>
  );
}

function TreeRows({
  projectId,
  entries,
  depth,
  expanded,
  onToggle,
  onOpenReport,
  onDelete,
}: {
  projectId: string;
  entries: FileEntry[];
  depth: number;
  expanded: Set<string>;
  onToggle: (path: string) => void;
  onOpenReport: (path: string) => void;
  onDelete: (path: string) => void;
}) {
  return (
    <>
      {entries.map((e) => {
        const indent = { paddingLeft: 14 + depth * 22 };
        const del = (
          <button
            className="icon-btn ftree-del"
            title={`Delete ${e.path}`}
            aria-label={`Delete ${e.path}`}
            onClick={(ev) => {
              ev.stopPropagation();
              if (window.confirm(`Delete "${e.path}" from the files dir?`)) onDelete(e.path);
            }}
          >
            <Trash2 size={13} />
          </button>
        );

        if (e.isDir) {
          const isReport = e.reportTitle !== undefined;
          const open = expanded.has(e.path);
          // Top-level rows always show the on-disk name; an experiment
          // folder's title surfaces as a hover tooltip. Nested report
          // folders show the report's own title.
          const exp = depth === 0 ? e.experiment : undefined;
          const showTitle = isReport && depth > 0;
          return (
            <div key={e.path}>
              <div
                className="ftree-row clickable"
                style={indent}
                title={exp?.title ?? (isReport ? e.reportTitle : undefined)}
                onClick={() => (isReport ? onOpenReport(e.path) : onToggle(e.path))}
              >
                <button
                  className={`ftree-chevron ${open ? "open" : ""}`}
                  aria-label={open ? `Collapse ${e.name}` : `Expand ${e.name}`}
                  onClick={(ev) => {
                    ev.stopPropagation();
                    onToggle(e.path);
                  }}
                >
                  <ChevronRight size={13} />
                </button>
                {showTitle ? (
                  <span className="ftree-title">{e.reportTitle}</span>
                ) : (
                  <span className="ftree-dirname">{e.name}/</span>
                )}
                {isReport && <span className="ftree-tag">report</span>}
                {exp?.latestRunStatus && (
                  <span className="ftree-status" title={exp.branchName}>
                    {exp.latestRunStatus}
                  </span>
                )}
                {del}
                <span className="ftree-date">
                  {new Date(e.modifiedAt).toLocaleDateString()}
                </span>
              </div>
              {open && (
                <div className="ftree-children">
                  <TreeRows
                    projectId={projectId}
                    entries={e.children ?? []}
                    depth={depth + 1}
                    expanded={expanded}
                    onToggle={onToggle}
                    onOpenReport={onOpenReport}
                    onDelete={onDelete}
                  />
                </div>
              )}
            </div>
          );
        }

        return (
          <div key={e.path} className="ftree-row" style={indent}>
            <span className="ftree-chevron spacer" />
            <a
              className="ftree-link"
              href={fileUrl(projectId, e.path)}
              target="_blank"
              rel="noopener noreferrer"
              title={e.path}
            >
              {IMAGE_RE.test(e.name) && (
                <img
                  className="ftree-thumb"
                  src={fileUrl(projectId, e.path)}
                  alt=""
                  loading="lazy"
                />
              )}
              <span className="ftree-name">{e.name}</span>
            </a>
            {del}
            <span className="ftree-size">{fmtBytes(e.size)}</span>
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

/** Middle-pane Files tab — an explorer over the project's files folder on
 * disk. Top-level folders correspond to experiments (named by slug), with the
 * reserved `project/` namespace for project-wide reports pinned first; a
 * folder with a top-level report.md additionally opens as a rendered report. */
export function FilesTab({
  project,
  files,
  onChanged,
}: {
  project: Project;
  files: ProjectFiles | null;
  onChanged: () => void;
}) {
  const [openPath, setOpenPath] = useState<string | null>(null);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());

  const openEntry = openPath && files ? findEntry(files.entries, openPath) : null;

  // The open report vanished from disk (deleted externally) — go back.
  useEffect(() => {
    if (openPath && files && !findEntry(files.entries, openPath)?.reportTitle)
      setOpenPath(null);
  }, [openPath, files]);

  const toggle = (path: string) =>
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });

  const remove = (path: string) => {
    void deleteFile(project.id, path)
      .catch(() => {})
      .finally(onChanged);
  };

  if (openEntry?.reportTitle) {
    return (
      <ReportView
        projectId={project.id}
        entry={openEntry}
        onBack={() => setOpenPath(null)}
        onDelete={() => {
          remove(openEntry.path);
          setOpenPath(null);
        }}
      />
    );
  }

  if (!files) {
    return (
      <div className="artifacts">
        <div className="settings-loading">
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
      expanded={expanded}
      onToggle={toggle}
      onOpenReport={setOpenPath}
      onDelete={remove}
    />
  );
  const repoUrl = `https://github.com/${project.githubOwner}/${project.githubRepo}`;
  return (
    <div className="artifacts">
      <div className="artifacts-col">
        <div className="files-meta">
          <a className="files-pill" href={repoUrl} target="_blank" rel="noopener noreferrer">
            <code>
              {project.githubOwner}/{project.githubRepo}
            </code>
            <ExternalLink size={12} />
          </a>
          <DirPath dir={files.dir} />
        </div>
        <p className="artifacts-hint">
          An explorer over this folder — the agent writes each experiment's reports and figures
          into the folder named for its slug (project-wide reports under <code>project/</code>),
          and you can drop in your own files.
        </p>
        {files.entries.length === 0 ? (
          <p className="files-empty">
            Nothing here yet. Ask the agent for a write-up of its findings — it saves each
            experiment's report folder (<code>report.md</code> + images) into the folder above.
          </p>
        ) : (
          <div className="files-card">
            {tree(projectNs)}
            {tree(experiments)}
            {other.length > 0 && (experiments.length > 0 || projectNs.length > 0) && (
              <div className="ftree-divider">Not linked to an experiment</div>
            )}
            {tree(other)}
            {files.truncated && (
              <p className="files-truncated">Listing truncated — the folder has more files.</p>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
