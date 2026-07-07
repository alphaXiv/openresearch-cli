// Mirror of openresearch.sh's AgentFileView: one file from the project clone,
// refractor-highlighted, opened as a right-pane tab from chat tool rows.

import { Code, FileText, RotateCw } from "lucide-react";
import { useEffect, useMemo, useState, type ReactNode } from "react";
import { refractor } from "refractor";
import { getProjectFile, type ProjectFile } from "../api";
import { detectSyntaxLanguageFromFilePath } from "../syntaxLanguage";
import { Md } from "./Md";

const HIGHLIGHT_MAX_BYTES = 300_000; // above this, skip tokenizing

interface HastNode {
  type: string;
  value?: string;
  tagName?: string;
  properties?: { className?: string[] };
  children?: HastNode[];
}

function hastToReact(node: HastNode, key: number): ReactNode {
  if (node.type === "text") return node.value ?? "";
  if (node.type !== "element") return null;
  return (
    <span key={key} className={(node.properties?.className ?? []).join(" ")}>
      {(node.children ?? []).map(hastToReact)}
    </span>
  );
}

function highlight(content: string, path: string): ReactNode {
  const lang = detectSyntaxLanguageFromFilePath(path);
  if (!lang || !refractor.registered(lang) || content.length > HIGHLIGHT_MAX_BYTES)
    return content;
  try {
    return (refractor.highlight(content, lang).children as HastNode[]).map(hastToReact);
  } catch {
    return content; // highlighting is best-effort
  }
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
    () => (data && !data.notFound ? highlight(data.content, path) : null),
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
