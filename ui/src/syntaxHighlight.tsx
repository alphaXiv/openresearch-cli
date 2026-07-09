// Shared refractor highlighting used by the file viewer (path → language) and
// chat markdown code blocks (fence info → language). Both render the resulting
// hast into <span class="token …"> nodes styled by the .token theme in
// styles.css.

import type { ReactNode } from "react";
import { refractor } from "refractor";

interface HastNode {
  type: string;
  value?: string;
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

/** Highlight `code` in `lang`, best-effort: returns the raw string when the
 * language isn't registered, the input is too large, or tokenizing throws. */
export function highlight(code: string, lang: string | null, maxBytes = 300_000): ReactNode {
  if (!lang || !refractor.registered(lang) || code.length > maxBytes) return code;
  try {
    return (refractor.highlight(code, lang).children as HastNode[]).map(hastToReact);
  } catch {
    return code;
  }
}
