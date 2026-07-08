import {
  ArrowLeft,
  Check,
  ChevronRight,
  Copy,
  ExternalLink,
  File,
  FileText,
  Folder,
  FolderGit2,
  FolderOpen,
  Trash2,
} from "lucide-react";
import { useEffect, useState } from "react";
import ReactMarkdown from "react-markdown";
import {
  artifactFileUrl,
  deleteArtifact,
  getArtifactReport,
  type ArtifactEntry,
  type Artifacts,
  type Project,
} from "../api";

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
function findEntry(entries: ArtifactEntry[], path: string): ArtifactEntry | null {
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
 * rewritten to the artifact file endpoint, scoped to the report's folder. */
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
    isExternalSrc(src)
      ? src
      : artifactFileUrl(projectId, `${folder}/${src.replace(/^\.?\//, "")}`);
  return (
    <div className="md report-md">
      <ReactMarkdown
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
  entry: ArtifactEntry;
  onBack: () => void;
  onDelete: () => void;
}) {
  const [markdown, setMarkdown] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    getArtifactReport(projectId, entry.path)
      .then((r) => setMarkdown(r.markdown))
      .catch((e) => setError(e instanceof Error ? e.message : String(e)));
  }, [projectId, entry.path, entry.modifiedAt]);

  return (
    <div className="report-view">
      <div className="report-view-head">
        <button className="report-back" onClick={onBack}>
          <ArrowLeft size={13} /> Artifacts
        </button>
        <span style={{ flex: 1 }} />
        <span className="report-date">{new Date(entry.modifiedAt).toLocaleString()}</span>
        <button
          className="icon-btn"
          title="Delete report folder"
          aria-label="Delete report folder"
          onClick={() => {
            if (window.confirm(`Delete the "${entry.path}" folder from the artifacts dir?`))
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
  );
}

/** The artifacts dir path, copyable — where the user (or agent) drops files. */
function DirPath({ dir }: { dir: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="artifacts-dir">
      <FolderOpen size={13} />
      <code>{dir}</code>
      <button
        className="icon-btn"
        title="Copy path"
        aria-label="Copy artifacts directory path"
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
  entries: ArtifactEntry[];
  depth: number;
  expanded: Set<string>;
  onToggle: (path: string) => void;
  onOpenReport: (path: string) => void;
  onDelete: (path: string) => void;
}) {
  return (
    <>
      {entries.map((e) => {
        const indent = { paddingLeft: 12 + depth * 18 };
        const del = (
          <button
            className="icon-btn artifact-tree-del"
            title={`Delete ${e.path}`}
            aria-label={`Delete ${e.path}`}
            onClick={(ev) => {
              ev.stopPropagation();
              if (window.confirm(`Delete "${e.path}" from the artifacts dir?`)) onDelete(e.path);
            }}
          >
            <Trash2 size={13} />
          </button>
        );

        if (e.isDir) {
          const isReport = e.reportTitle !== undefined;
          const open = expanded.has(e.path);
          return (
            <div key={e.path}>
              <div
                className="artifact-tree-row clickable"
                style={indent}
                onClick={() => (isReport ? onOpenReport(e.path) : onToggle(e.path))}
              >
                <button
                  className={`artifact-tree-chevron ${open ? "open" : ""}`}
                  aria-label={open ? `Collapse ${e.name}` : `Expand ${e.name}`}
                  onClick={(ev) => {
                    ev.stopPropagation();
                    onToggle(e.path);
                  }}
                >
                  <ChevronRight size={13} />
                </button>
                {isReport ? <FileText size={14} /> : <Folder size={14} />}
                <span className={`artifact-tree-name ${isReport ? "report" : ""}`}>
                  {e.reportTitle || e.name}
                </span>
                {del}
                <span className="report-date">
                  {new Date(e.modifiedAt).toLocaleDateString()}
                </span>
              </div>
              {open && (
                <TreeRows
                  projectId={projectId}
                  entries={e.children ?? []}
                  depth={depth + 1}
                  expanded={expanded}
                  onToggle={onToggle}
                  onOpenReport={onOpenReport}
                  onDelete={onDelete}
                />
              )}
            </div>
          );
        }

        return (
          <div key={e.path} className="artifact-tree-row" style={indent}>
            <span className="artifact-tree-chevron spacer" />
            <a
              className="artifact-file-link"
              href={artifactFileUrl(projectId, e.path)}
              target="_blank"
              rel="noopener noreferrer"
              title={e.path}
            >
              {IMAGE_RE.test(e.name) ? (
                <img
                  className="artifact-thumb"
                  src={artifactFileUrl(projectId, e.path)}
                  alt=""
                  loading="lazy"
                />
              ) : (
                <File size={14} />
              )}
              <span className="artifact-tree-name">{e.name}</span>
            </a>
            {del}
            <span className="report-date">{fmtBytes(e.size)}</span>
          </div>
        );
      })}
    </>
  );
}

/** Right-pane Artifacts tab — an explorer over the project's artifacts folder
 * on disk. Every entry is an artifact; a folder with a top-level report.md
 * additionally opens as a rendered report. */
export function ArtifactsTab({
  project,
  artifacts,
  onChanged,
}: {
  project: Project;
  artifacts: Artifacts | null;
  onChanged: () => void;
}) {
  const [openPath, setOpenPath] = useState<string | null>(null);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());

  const openEntry = openPath && artifacts ? findEntry(artifacts.entries, openPath) : null;

  // The open report vanished from disk (deleted externally) — go back.
  useEffect(() => {
    if (openPath && artifacts && !findEntry(artifacts.entries, openPath)?.reportTitle)
      setOpenPath(null);
  }, [openPath, artifacts]);

  const toggle = (path: string) =>
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });

  const remove = (path: string) => {
    void deleteArtifact(project.id, path)
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

  if (!artifacts) {
    return (
      <div className="artifacts">
        <div className="settings-loading">
          <span className="spinner" /> Loading artifacts…
        </div>
      </div>
    );
  }

  const repoUrl = `https://github.com/${project.githubOwner}/${project.githubRepo}`;
  return (
    <div className="artifacts">
      <section>
        <h3 className="artifacts-heading">
          <FolderGit2 size={13} /> Repository
        </h3>
        <a className="artifacts-repo" href={repoUrl} target="_blank" rel="noopener noreferrer">
          {project.githubOwner}/{project.githubRepo}
          <ExternalLink size={12} />
        </a>
      </section>

      <section>
        <h3 className="artifacts-heading">
          <FolderOpen size={13} /> Artifacts
        </h3>
        <DirPath dir={artifacts.dir} />
        <p className="artifacts-hint">
          An explorer over this folder — the agent writes reports and figures into it, and you
          can drop in your own files.
        </p>
        {artifacts.entries.length === 0 ? (
          <p className="artifacts-empty">
            Nothing here yet. Ask the agent for a write-up of its findings — it saves report
            folders (<code>report.md</code> + images) into the folder above.
          </p>
        ) : (
          <div className="artifact-tree">
            <TreeRows
              projectId={project.id}
              entries={artifacts.entries}
              depth={0}
              expanded={expanded}
              onToggle={toggle}
              onOpenReport={setOpenPath}
              onDelete={remove}
            />
            {artifacts.truncated && (
              <p className="artifacts-hint" style={{ padding: "6px 12px" }}>
                Listing truncated — the folder has more files.
              </p>
            )}
          </div>
        )}
      </section>
    </div>
  );
}
