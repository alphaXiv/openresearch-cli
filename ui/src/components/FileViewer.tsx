// Mirror of openresearch.sh's AgentFileView: one file from the project —
// a branch's committed copy when the tab carries a ref, else the chat
// session's worktree, else the hub clone — refractor-highlighted, opened as
// a right-pane tab from chat tool rows or the code browser.

import { Code, FileText, RotateCw } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { getProjectFile, type ProjectFile } from "../api";
import { detectSyntaxLanguageFromFilePath } from "../syntaxLanguage";
import { highlight } from "../syntaxHighlight";
import { Md } from "./Md";

function highlightFile(content: string, path: string) {
  return highlight(content, detectSyntaxLanguageFromFilePath(path));
}

export function FileViewer({
  projectId,
  path,
  sessionId,
  gitRef,
  onOpenFile,
}: {
  projectId: string;
  path: string;
  /** Chat session whose worktree holds the file (absent → hub clone). */
  sessionId?: string;
  /** Branch whose committed copy to show — overrides the live checkout.
   * (Named gitRef because `ref` is reserved on React components.) */
  gitRef?: string;
  /** Open a linked file as another tab (rendered-markdown links). */
  onOpenFile?: (path: string, sessionId?: string, ref?: string) => void;
}) {
  const [data, setData] = useState<ProjectFile | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [nonce, setNonce] = useState(0);
  // Markdown renders by default; the header toggle shows the raw source.
  const isMarkdown = /\.(md|mdx|markdown)$/i.test(path);
  const [showSource, setShowSource] = useState(false);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    getProjectFile(projectId, path, { sessionId, ref: gitRef })
      .then((d) => {
        if (cancelled) return;
        setData(d);
        setError(null);
      })
      .catch((e: Error) => {
        if (!cancelled) setError(e.message);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [projectId, path, sessionId, gitRef, nonce]);

  const rendered = useMemo(
    () => (data && !data.notFound ? highlightFile(data.content, path) : null),
    [data, path],
  );

  // One number per source line; a trailing newline ends a line, it doesn't
  // start an empty one.
  const lineCount =
    data && !data.notFound && data.content
      ? data.content.split("\n").length - (data.content.endsWith("\n") ? 1 : 0)
      : 0;

  const notFoundCopy = (d: ProjectFile) => {
    if (gitRef) return `File not found on branch ${gitRef}.`;
    if (sessionId && d.root === "clone")
      return "This session's worktree isn't available, and the file isn't in the project clone.";
    return `File not found in the ${d.root === "worktree" ? "session's worktree" : "project clone"}.`;
  };

  return (
    <div className="file-view">
      <div className="file-view-header">
        <FileText size={13} style={{ flexShrink: 0 }} />
        <code className="file-view-path" title={path}>
          {path}
        </code>
        {gitRef && (
          <code className="file-view-ref" title={`Committed state of ${gitRef}`}>
            {gitRef}
          </code>
        )}
        {isMarkdown && (
          <button
            className={`icon-btn ${showSource ? "active" : ""}`}
            title={showSource ? "Rendered view" : "Source view"}
            aria-label={showSource ? "Rendered view" : "Source view"}
            onClick={() => setShowSource((s) => !s)}
          >
            <Code size={13} />
          </button>
        )}
        <button
          className="icon-btn"
          title="Reload file"
          aria-label="Reload file"
          onClick={() => setNonce((n) => n + 1)}
        >
          {loading ? <span className="spinner" /> : <RotateCw size={13} />}
        </button>
      </div>
      <div className="file-view-body">
        {error ? (
          <div className="file-view-note">Failed to load file: {error}</div>
        ) : data === null ? (
          <div className="file-view-note">Loading…</div>
        ) : data.notFound ? (
          <div className="file-view-note">{notFoundCopy(data)}</div>
        ) : (
          <>
            {!gitRef && sessionId && data.root === "clone" && (
              <div className="file-view-note">
                This session's worktree isn't available — showing the project clone's copy.
              </div>
            )}
            {isMarkdown && !showSource ? (
              <div className="file-view-md">
                <Md
                  text={data.content}
                  onOpenFile={onOpenFile && ((p) => onOpenFile(p, sessionId, gitRef))}
                />
              </div>
            ) : (
              <div className="file-view-codewrap">
                {/* No numbers for an empty file — an empty gutter is just a
                    stray bordered strip. */}
                {lineCount > 0 && (
                  <pre className="file-view-gutter" aria-hidden="true">
                    {Array.from({ length: lineCount }, (_, i) => i + 1).join("\n")}
                  </pre>
                )}
                <pre className="file-view-code">
                  <code>{rendered}</code>
                </pre>
              </div>
            )}
            {data.truncated && (
              <div className="file-view-note">File truncated — showing the first 512 KB.</div>
            )}
          </>
        )}
      </div>
    </div>
  );
}
