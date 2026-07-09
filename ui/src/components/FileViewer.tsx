// Mirror of openresearch.sh's AgentFileView: one file from the project clone,
// refractor-highlighted, opened as a right-pane tab from chat tool rows.

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
  onOpenFile,
}: {
  projectId: string;
  path: string;
  /** Open a linked file as another tab (rendered-markdown links). */
  onOpenFile?: (path: string) => void;
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
    getProjectFile(projectId, path)
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
  }, [projectId, path, nonce]);

  const rendered = useMemo(
    () => (data && !data.notFound ? highlightFile(data.content, path) : null),
    [data, path],
  );

  return (
    <div className="file-view">
      <div className="file-view-header">
        <FileText size={13} style={{ flexShrink: 0 }} />
        <code className="file-view-path" title={path}>
          {path}
        </code>
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
          <div className="file-view-note">File not found in the project clone.</div>
        ) : (
          <>
            {isMarkdown && !showSource ? (
              <div className="file-view-md">
                <Md text={data.content} onOpenFile={onOpenFile} />
              </div>
            ) : (
              <pre className="file-view-code">
                <code>{rendered}</code>
              </pre>
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
