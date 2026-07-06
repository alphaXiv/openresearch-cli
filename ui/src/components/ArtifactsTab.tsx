import {
  ArrowLeft,
  Check,
  Copy,
  ExternalLink,
  File,
  FileText,
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
  type ArtifactFile,
  type ArtifactReport,
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

/** Report markdown with report-relative image/link paths (`images/...`)
 * rewritten to the artifact file endpoint, scoped to the report's folder. */
function ReportMd({
  projectId,
  reportName,
  markdown,
}: {
  projectId: string;
  reportName: string;
  markdown: string;
}) {
  const resolve = (src: string) =>
    isExternalSrc(src)
      ? src
      : artifactFileUrl(projectId, `${reportName}/${src.replace(/^\.?\//, "")}`);
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
  report,
  onBack,
  onDelete,
}: {
  projectId: string;
  report: ArtifactReport;
  onBack: () => void;
  onDelete: () => void;
}) {
  const [markdown, setMarkdown] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    getArtifactReport(projectId, report.name)
      .then((r) => setMarkdown(r.markdown))
      .catch((e) => setError(e instanceof Error ? e.message : String(e)));
  }, [projectId, report.name, report.modifiedAt]);

  return (
    <div className="report-view">
      <div className="report-view-head">
        <button className="btn sm ghost" onClick={onBack}>
          <ArrowLeft size={12} /> Reports
        </button>
        <div className="report-view-title">
          <span>{report.title}</span>
          <span className="report-date">{new Date(report.modifiedAt).toLocaleString()}</span>
        </div>
        <button
          className="icon-btn"
          title="Delete report folder"
          aria-label="Delete report folder"
          onClick={() => {
            if (window.confirm(`Delete the "${report.name}" folder from the artifacts dir?`))
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
        <ReportMd projectId={projectId} reportName={report.name} markdown={markdown} />
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

/** Right-pane Artifacts tab — a live view of the project's artifacts folder
 * on disk. Subfolders with a report.md render as reports; everything else
 * (figures, CSVs, whatever the user drops in) is listed as files. */
export function ArtifactsTab({
  project,
  artifacts,
  onChanged,
}: {
  project: Project;
  artifacts: Artifacts | null;
  onChanged: () => void;
}) {
  const [openName, setOpenName] = useState<string | null>(null);
  const open = openName
    ? (artifacts?.reports.find((r) => r.name === openName) ?? null)
    : null;

  // The open report vanished from disk (deleted externally) — go back.
  useEffect(() => {
    if (openName && artifacts && !artifacts.reports.some((r) => r.name === openName))
      setOpenName(null);
  }, [openName, artifacts]);

  const remove = (path: string) => {
    void deleteArtifact(project.id, path)
      .catch(() => {})
      .finally(onChanged);
  };

  if (open) {
    return (
      <ReportView
        projectId={project.id}
        report={open}
        onBack={() => setOpenName(null)}
        onDelete={() => {
          remove(open.name);
          setOpenName(null);
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
  const empty = artifacts.reports.length === 0 && artifacts.files.length === 0;
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
          <FolderOpen size={13} /> Local folder
        </h3>
        <DirPath dir={artifacts.dir} />
        <p className="artifacts-hint">
          Everything in this folder shows up here — the agent writes reports and figures into
          it, and you can drop in your own files.
        </p>
      </section>

      {empty ? (
        <p className="artifacts-empty">
          Nothing here yet. Ask the agent for a write-up of its findings — it saves report
          folders (<code>report.md</code> + images) into the folder above.
        </p>
      ) : (
        <>
          {artifacts.reports.length > 0 && (
            <section>
              <h3 className="artifacts-heading">
                <FileText size={13} /> Reports
                <span className="artifacts-count">{artifacts.reports.length}</span>
              </h3>
              <ul className="report-list">
                {artifacts.reports.map((r) => (
                  <li key={r.name}>
                    <button className="report-row" onClick={() => setOpenName(r.name)}>
                      <span className="report-row-title">{r.title}</span>
                      <span className="report-date">
                        {new Date(r.modifiedAt).toLocaleDateString()}
                      </span>
                    </button>
                  </li>
                ))}
              </ul>
            </section>
          )}

          {artifacts.files.length > 0 && (
            <section>
              <h3 className="artifacts-heading">
                <File size={13} /> Files
                <span className="artifacts-count">{artifacts.files.length}</span>
              </h3>
              <ul className="report-list">
                {artifacts.files.map((f: ArtifactFile) => (
                  <li key={f.path}>
                    <div className="report-row artifact-file-row">
                      <a
                        className="artifact-file-link"
                        href={artifactFileUrl(project.id, f.path)}
                        target="_blank"
                        rel="noopener noreferrer"
                        title={f.path}
                      >
                        {IMAGE_RE.test(f.path) && (
                          <img
                            className="artifact-thumb"
                            src={artifactFileUrl(project.id, f.path)}
                            alt=""
                            loading="lazy"
                          />
                        )}
                        <span className="report-row-title">{f.path}</span>
                      </a>
                      <span className="report-date">{fmtBytes(f.size)}</span>
                      <button
                        className="icon-btn"
                        title="Delete file"
                        aria-label={`Delete ${f.path}`}
                        onClick={() => {
                          if (window.confirm(`Delete "${f.path}" from the artifacts dir?`))
                            remove(f.path);
                        }}
                      >
                        <Trash2 size={13} />
                      </button>
                    </div>
                  </li>
                ))}
              </ul>
              {artifacts.truncated && (
                <p className="artifacts-hint">Listing truncated — the folder has more files.</p>
              )}
            </section>
          )}
        </>
      )}
    </div>
  );
}
