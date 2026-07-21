// Chat markdown with file mentions, mirroring openresearch.sh's
// MarkdownContent: `<file path="..." lines="20-40"/>` tags (and plain relative
// links) render as chips that open the file as a right-pane tab.

import { Check, Copy, FileCode } from "lucide-react";
import { useState, type ReactNode } from "react";
import ReactMarkdown from "react-markdown";
import rehypeKatex from "rehype-katex";
import remarkGfm from "remark-gfm";
import remarkMath from "remark-math";
import { resolveSyntaxLanguage } from "../syntaxLanguage";
import { highlight } from "../syntaxHighlight";
import "katex/dist/katex.min.css";

// Chat blocks are short; cap tokenizing well below the file viewer's limit.
const HIGHLIGHT_MAX_BYTES = 100_000;

/** A fenced code block: syntax-highlighted body + a copy button. */
function CodeBlock({ code, lang }: { code: string; lang: string | null }) {
  const [copied, setCopied] = useState(false);
  const copy = () => {
    navigator.clipboard?.writeText(code).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    });
  };
  return (
    <div className="md-code">
      <button className="md-code-copy" title="Copy" aria-label="Copy code" onClick={copy}>
        {copied ? <Check size={13} /> : <Copy size={13} />}
      </button>
      <pre>
        <code>{highlight(code, lang, HIGHLIGHT_MAX_BYTES)}</code>
      </pre>
    </div>
  );
}

interface MdastNode {
  children?: MdastNode[];
  data?: { hName?: string; hProperties?: Record<string, string> };
  type: string;
  value?: string;
}

/** Pull `name="value"` (or single-quoted) attributes off a tag's attribute run. */
function parseTagAttrs(attrs: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const attr of attrs.matchAll(/([\w-]+)=(["'])(.*?)\2/g)) {
    const key = attr[1];
    if (key) out[key.toLowerCase()] = attr[3] ?? "";
  }
  return out;
}

/** Split raw-html text around `<file .../>` tags into text + custom nodes. */
function parseFileMentionHtml(value: string): MdastNode[] | null {
  const regex = /<file\b([^>]*?)\/?>/gi;
  const nodes: MdastNode[] = [];
  let lastIndex = 0;
  let matched = false;

  for (const match of value.matchAll(regex)) {
    const attrs = parseTagAttrs(match[1] ?? "");
    // A tag with no path is almost certainly not ours — leave it as text.
    if (!attrs["path"]) continue;

    matched = true;
    if (match.index > lastIndex) {
      nodes.push({ type: "text", value: value.slice(lastIndex, match.index) });
    }
    nodes.push({
      children: [],
      data: { hName: "file-mention", hProperties: attrs },
      type: "fileMention",
    });
    lastIndex = match.index + match[0].length;
  }

  if (!matched) return null;
  if (lastIndex < value.length) {
    nodes.push({ type: "text", value: value.slice(lastIndex) });
  }
  return nodes;
}

function replaceFileMentions(parent: MdastNode) {
  const children = parent.children;
  if (!children) return;
  for (let i = 0; i < children.length; i += 1) {
    const child = children[i];
    if (!child) continue;
    if (child.type === "html" && typeof child.value === "string") {
      const replacement = parseFileMentionHtml(child.value);
      if (replacement) {
        children.splice(i, 1, ...replacement);
        i += replacement.length - 1;
        continue;
      }
    }
    replaceFileMentions(child);
  }
}

function remarkFileMentions() {
  return (tree: MdastNode) => replaceFileMentions(tree);
}

function FileChip({
  path,
  lines,
  onOpenFile,
}: {
  path: string;
  lines?: string;
  onOpenFile?: (path: string) => void;
}) {
  const name = path.split("/").pop() || path;
  // `lines` may be a single line or a range ("20-40"); show the first.
  const line = lines ? Number.parseInt(lines, 10) || undefined : undefined;
  const label = line != null ? `${name}:${line}` : name;
  return (
    <button
      className="file-chip"
      title={`Open ${path}`}
      onClick={() => onOpenFile?.(path)}
      disabled={!onOpenFile}
    >
      <FileCode size={12} />
      <span className="file-chip-label">{label}</span>
    </button>
  );
}

// Matches regions the math normalizer must not touch: fenced code blocks
// (tolerating an unclosed fence mid-stream) and inline code spans.
const CODE_REGIONS = /(```[\s\S]*?(?:```|$)|~~~[\s\S]*?(?:~~~|$)|`[^`\n]*`)/g;

/** Rewrite `\(...\)` / `\[...\]` math delimiters to remark-math's `$` forms.
 *
 * Agents emit LaTeX with backslash delimiters, which plain markdown mangles:
 * `\(` parses as an escaped paren and `_` as emphasis. remark-math only
 * recognizes dollar delimiters, so convert before parsing — skipping code
 * blocks and inline code, where backslashes are literal. */
export function normalizeMathDelimiters(text: string): string {
  if (!text.includes("\\(") && !text.includes("\\[")) return text;
  return text
    .split(CODE_REGIONS)
    .map((seg, i) => {
      if (i % 2 === 1) return seg; // odd segments are code — leave untouched
      return seg
        .replace(/\\\[([\s\S]+?)\\\]/g, (_, inner: string) => `$$${inner}$$`)
        .replace(/\\\(([\s\S]+?)\\\)/g, (_, inner: string) => `$${inner}$`);
    })
    .join("");
}

/** A link target that is a file path rather than a web URL. */
function isFileHref(href: string): boolean {
  if (/^[a-z][a-z0-9+.-]*:/i.test(href)) return false; // has a scheme
  if (href.startsWith("#") || href.startsWith("//")) return false;
  return true;
}

/** Shared `code`/`pre` renderers: fenced blocks (language-*) become
 * highlighted CodeBlocks with a copy button; inline code stays a plain
 * <code> chip. The <pre> wrapper is handled inside CodeBlock, so
 * react-markdown's is unwrapped. Reused by the Files tab's report renderer. */
export const mdCodeComponents: Record<string, (props: any) => ReactNode> = {
  code: ({ node: _node, className, children, ...rest }: any) => {
    const cls: string = className ?? "";
    const match = /language-(\w+)/.exec(cls);
    const raw = String(children ?? "").replace(/\n$/, "");
    const isBlock = match != null || raw.includes("\n");
    if (!isBlock) {
      return (
        <code className={cls} {...rest}>
          {children}
        </code>
      );
    }
    const lang = match ? resolveSyntaxLanguage(match[1]) : null;
    return <CodeBlock code={raw} lang={lang} />;
  },
  pre: ({ children }: any) => <>{children}</>,
};

export function Md({ text, onOpenFile }: { text: string; onOpenFile?: (path: string) => void }) {
  const components: Record<string, (props: any) => ReactNode> = {
    "file-mention": (props) => (
      <FileChip path={props.path} lines={props.lines} onOpenFile={onOpenFile} />
    ),
    a: ({ node: _node, href, children, ...rest }) => {
      // Agents sometimes link files as plain markdown links; open those as
      // file tabs instead of navigating the dashboard away.
      if (href && isFileHref(href) && onOpenFile) {
        return <FileChip path={decodeURI(href)} onOpenFile={onOpenFile} />;
      }
      return (
        <a href={href} target="_blank" rel="noopener noreferrer" {...rest}>
          {children}
        </a>
      );
    },
    ...mdCodeComponents,
  };

  return (
    <div className="md">
      <ReactMarkdown
        remarkPlugins={[remarkGfm, remarkMath, remarkFileMentions]}
        rehypePlugins={[rehypeKatex]}
        components={components as any}
      >
        {normalizeMathDelimiters(text)}
      </ReactMarkdown>
    </div>
  );
}
