// Shared source-code block: line-number gutter + refractor-highlighted
// content, used by the repo file viewer and the Files tab preview. Style
// scoping note: syntax token colors apply under a `.file-view` ancestor.
import { useMemo } from "react";
import { detectSyntaxLanguageFromFilePath } from "../syntaxLanguage";
import { highlight } from "../syntaxHighlight";

export function CodeView({ text, path }: { text: string; path: string }) {
  const rendered = useMemo(
    () => highlight(text, detectSyntaxLanguageFromFilePath(path)),
    [text, path],
  );
  // One number per source line; a trailing newline ends a line, it doesn't
  // start an empty one.
  const lineCount = text ? text.split("\n").length - (text.endsWith("\n") ? 1 : 0) : 0;
  return (
    <div className="file-view-codewrap">
      {/* No numbers for an empty file — an empty gutter is just a stray
          bordered strip. */}
      {lineCount > 0 && (
        <pre className="file-view-gutter" aria-hidden="true">
          {Array.from({ length: lineCount }, (_, i) => i + 1).join("\n")}
        </pre>
      )}
      <pre className="file-view-code">
        <code>{rendered}</code>
      </pre>
    </div>
  );
}
