// Mirror of openresearch.sh's GitDiff: per-file collapsible cards over
// react-diff-view's unified view, with refractor syntax highlighting.

import "react-diff-view/style/index.css";
import { ChevronDown, ChevronRight } from "lucide-react";
import { useMemo, useState } from "react";
import {
  Diff,
  type ChangeData,
  type FileData,
  type HunkTokens,
  type RenderGutter,
  markEdits,
  parseDiff,
  tokenize,
} from "react-diff-view";
import { refractor } from "refractor";
import { detectSyntaxLanguageFromFilePath } from "../syntaxLanguage";

const HIGHLIGHT_MAX = 2000; // above this many changed lines, skip tokenizing

const REACT_DIFF_VIEW_REFRACTOR = {
  highlight(code: string, language: string) {
    return refractor.highlight(code, language).children;
  },
};

function getUnifiedLineNumber(change: ChangeData): number {
  if (change.type === "normal") return change.newLineNumber;
  return change.lineNumber;
}

function countChanges(file: FileData) {
  let additions = 0;
  let deletions = 0;
  for (const hunk of file.hunks) {
    for (const change of hunk.changes) {
      if (change.type === "insert") additions++;
      else if (change.type === "delete") deletions++;
    }
  }
  return { additions, deletions };
}

function getHighlightPath(file: FileData): string | null {
  if (file.newPath === "/dev/null") return file.oldPath;
  if (file.oldPath === "/dev/null") return file.newPath;
  return file.newPath;
}

function formatDiffFilePath(file: FileData): string {
  switch (file.type) {
    case "delete":
      return file.oldPath;
    case "add":
    case "modify":
      return file.newPath;
    case "rename":
    case "copy":
      return `${file.oldPath} → ${file.newPath}`;
  }
}

function tokenizeDiffFile(file: FileData): HunkTokens {
  const enhancers = [markEdits(file.hunks, { type: "line" })];
  const language = detectSyntaxLanguageFromFilePath(getHighlightPath(file));
  if (language && refractor.registered(language)) {
    return tokenize(file.hunks, {
      enhancers,
      highlight: true,
      language,
      refractor: REACT_DIFF_VIEW_REFRACTOR,
    });
  }
  return tokenize(file.hunks, { enhancers, highlight: false });
}

const renderUnifiedGutter: RenderGutter = ({ change, side }) => {
  if (side === "old") return null;
  return getUnifiedLineNumber(change);
};

function formatBytes(bytes: number): string {
  if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / 1024).toFixed(1)} KB`;
}

export function TruncatedDiffNotice({
  bytesRead,
  byteLimit,
}: {
  bytesRead: number;
  byteLimit: number;
}) {
  return (
    <div className="truncated-notice">
      <h4>Diff too large to display</h4>
      <p>
        This diff is larger than {formatBytes(byteLimit)} ({formatBytes(bytesRead)} read). View it
        locally with git.
      </p>
    </div>
  );
}

function DiffFileCard({
  file,
  defaultExpanded,
}: {
  file: FileData;
  defaultExpanded: boolean;
}) {
  const [expanded, setExpanded] = useState(defaultExpanded);
  const { additions, deletions } = useMemo(() => countChanges(file), [file]);
  const shouldTokenize = expanded && additions + deletions <= HIGHLIGHT_MAX;
  const tokens = useMemo<HunkTokens | undefined>(() => {
    if (!shouldTokenize) return undefined;
    try {
      return tokenizeDiffFile(file);
    } catch {
      return undefined; // tokenizing is best-effort
    }
  }, [file, shouldTokenize]);

  return (
    <section className="diff-file-card">
      <button className="diff-file-header" onClick={() => setExpanded((e) => !e)}>
        <span className="chev">
          {expanded ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
        </span>
        <span className="path">
          <code>{formatDiffFilePath(file)}</code>
        </span>
        <span className="stats">
          <span className="diff-stat-add">+{additions}</span>
          <span className="diff-stat-del">−{deletions}</span>
        </span>
      </button>
      {expanded &&
        (file.hunks.length === 0 ? (
          <div className="diff-empty">No textual diff for this file.</div>
        ) : (
          <Diff
            className="openresearch-diff-file"
            diffType={file.type}
            gutterType="default"
            hunks={file.hunks}
            renderGutter={renderUnifiedGutter}
            tokens={tokens}
            viewType="unified"
          />
        ))}
    </section>
  );
}

export function GitDiff({ diff, className }: { diff: string; className?: string }) {
  const files = useMemo<FileData[]>(() => {
    if (!diff.trim()) return [];
    try {
      return parseDiff(diff, { nearbySequences: "zip" });
    } catch {
      return []; // malformed diff → empty state
    }
  }, [diff]);

  if (files.length === 0) return <div className="diff-empty">No changes.</div>;

  return (
    <div className={className ? `openresearch-diff ${className}` : "openresearch-diff"}>
      {files.map((file, i) => (
        <DiffFileCard
          key={`${file.oldPath}→${file.newPath}#${i}`}
          file={file}
          defaultExpanded={i === 0}
        />
      ))}
    </div>
  );
}
