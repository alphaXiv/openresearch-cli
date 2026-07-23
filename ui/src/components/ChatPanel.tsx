import {
  Check,
  ChevronRight,
  CornerDownLeft,
  FlaskConical,
  FolderGit2,
  FolderOpen,
  HelpCircle,
  MoreHorizontal,
  PanelLeft,
  Plus,
  SlidersHorizontal,
  Users,
  X,
} from "lucide-react";
import {
  memo,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useReducer,
  useRef,
  useState,
} from "react";
import { Wordmark } from "./Wordmark";
import {
  chatAttachmentUrl,
  createChatSession,
  deleteChatSession,
  getChatMessages,
  getSkills,
  interruptChat,
  listChatSessions,
  renameChatSession,
  respondChat,
  sendChatMessage,
  setChatSessionArchived,
  type ChatImageAttachment,
  type ChatMessage,
  type ChatPart,
  type ChatPrompt,
  type ChatSession,
  type Harness,
  type PromptAnswer,
  type SkillInfo,
} from "../api";
import { onChatEvent } from "../events";
import { Md } from "./Md";
import { PlanStrip } from "./PlanStrip";
import { SETTINGS_NAV, type SettingsTab } from "./SettingsPage";
import { SkillMenu } from "./SkillMenu";
import {
  defaultSelection,
  HARNESS_LABELS,
  ModelPicker,
  OptionPicker,
  usePopover,
  type ModelSelection,
} from "./ModelPicker";
import { ContextMeter } from "./ContextMeter";

const SELECTION_STORAGE_KEY = "orx:agent-selection";

function loadSelection(): ModelSelection | null {
  try {
    const raw = localStorage.getItem(SELECTION_STORAGE_KEY);
    return raw ? (JSON.parse(raw) as ModelSelection) : null;
  } catch {
    return null;
  }
}

// --- chat state --------------------------------------------------------------

interface ChatState {
  messagesBySession: Record<string, ChatMessage[]>;
  busySessions: Set<string>;
}

type Action =
  | { type: "reset" }
  | { type: "seed"; sessionId: string; messages: ChatMessage[] }
  | { type: "upsertMessage"; sessionId: string; message: ChatMessage }
  | { type: "optimisticUser"; sessionId: string; text: string; imageUrls: string[] }
  | { type: "busy"; sessionId: string; busy: boolean }
  | { type: "seedBusy"; sessions: string[] }
  | { type: "forget"; sessionId: string };

const LOCAL_PREFIX = "local-";

function upsertMessage(list: ChatMessage[], message: ChatMessage): ChatMessage[] {
  const i = list.findIndex((m) => m.id === message.id);
  if (i >= 0) {
    const next = list.slice();
    next[i] = message;
    return next;
  }
  // The server's copy of the user message replaces the optimistic local one.
  const cleaned =
    message.role === "user" ? list.filter((m) => !m.id.startsWith(LOCAL_PREFIX)) : list;
  return [...cleaned, message];
}

function reducer(state: ChatState, action: Action): ChatState {
  switch (action.type) {
    case "reset":
      return { messagesBySession: {}, busySessions: new Set() };
    case "seed":
      return {
        ...state,
        messagesBySession: { ...state.messagesBySession, [action.sessionId]: action.messages },
      };
    case "upsertMessage": {
      const list = state.messagesBySession[action.sessionId] ?? [];
      return {
        ...state,
        messagesBySession: {
          ...state.messagesBySession,
          [action.sessionId]: upsertMessage(list, action.message),
        },
      };
    }
    case "optimisticUser": {
      const list = state.messagesBySession[action.sessionId] ?? [];
      const parts: ChatPart[] = action.text
        ? [{ id: "p0", type: "text", text: action.text }]
        : [];
      // Data URLs stand in until the server's copy arrives with file names.
      action.imageUrls.forEach((url, i) =>
        parts.push({ id: `img${i}`, type: "image", text: url }),
      );
      const msg: ChatMessage = {
        id: `${LOCAL_PREFIX}${Date.now()}`,
        role: "user",
        parts,
        createdAt: Date.now(),
      };
      return {
        ...state,
        messagesBySession: { ...state.messagesBySession, [action.sessionId]: [...list, msg] },
      };
    }
    case "busy": {
      const busySessions = new Set(state.busySessions);
      if (action.busy) busySessions.add(action.sessionId);
      else busySessions.delete(action.sessionId);
      return { ...state, busySessions };
    }
    case "seedBusy":
      return { ...state, busySessions: new Set(action.sessions) };
    case "forget": {
      // Deleted session: drop its transcript and busy flag so a same-id event
      // arriving late can't render stale state.
      const messagesBySession = { ...state.messagesBySession };
      delete messagesBySession[action.sessionId];
      const busySessions = new Set(state.busySessions);
      busySessions.delete(action.sessionId);
      return { messagesBySession, busySessions };
    }
  }
}

// --- rendering ---------------------------------------------------------------

/** Which detected agent the first message will run on — keeps autodetection
 * legible at the moment the user first types. */
function EmptyStateAgentHint({
  harnesses,
  selection,
}: {
  harnesses: Harness[];
  selection: ModelSelection | null;
}) {
  if (harnesses.length === 0) return null; // still detecting
  const h = selection ? harnesses.find((x) => x.id === selection.harness) : undefined;
  if (!h) {
    return (
      <p className="chat-empty-hint">
        No coding agent detected on this machine — install Claude Code, Codex or opencode and
        sign in, then re-open the model picker below.
      </p>
    );
  }
  return (
    <p className="chat-empty-hint">
      Chatting with {h.name}
      {h.account ? ` as ${h.account}` : ""} — detected automatically, switch in the model picker
      below.
    </p>
  );
}

function toolStatusClass(status: string | undefined): string {
  if (status === "error") return "tool-status error";
  if (status === "completed") return "tool-status";
  return "tool-status running"; // running = in-flight
}

function relTime(ts: number | undefined): string {
  if (!ts) return "";
  const s = Math.max(0, Math.floor((Date.now() - ts) / 1000));
  if (s < 60) return "now";
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h`;
  return `${Math.floor(h / 24)}d`;
}

/** The last path segment, for compact display ("src/a/b.rs" → "b.rs"). */
function baseName(path: string): string {
  const trimmed = path.replace(/\/+$/, "");
  return trimmed.slice(trimmed.lastIndexOf("/") + 1) || trimmed;
}

/** Claude-desktop-style one-liner: a verb + target, e.g. "Read hello.py",
 * "Ran echo hello". Falls back to the raw tool name. */
function toolLine(part: ChatPart): string {
  const tool = part.tool ?? "tool";
  const input = part.state?.input ?? {};
  const cmd = typeof input.command === "string" ? input.command : null;
  const fp = typeof input.filePath === "string" ? input.filePath : null;
  const desc = typeof input.description === "string" ? input.description : null;
  switch (tool) {
    case "Bash":
    case "bash":
      return cmd ? `Ran ${cmd}` : "Ran command";
    case "Read":
      return fp ? `Read ${baseName(fp)}` : "Read file";
    case "Edit":
    case "Write":
    case "NotebookEdit":
      return fp ? `Edited ${baseName(fp)}` : "Edited file";
    case "Grep":
      return typeof input.pattern === "string" ? `Searched “${input.pattern}”` : "Searched";
    case "Glob":
      return typeof input.pattern === "string" ? `Found ${input.pattern}` : "Listed files";
    case "WebSearch": {
      // Codex web-tool actions: search {query}, openPage {url},
      // findInPage {pattern, url} — query is empty for the latter two.
      if (typeof input.query === "string" && input.query)
        return `Searched the web: “${input.query}”`;
      const url = typeof input.url === "string" ? input.url : null;
      if (typeof input.pattern === "string" && input.pattern && url)
        return `Searched “${input.pattern}” in ${url}`;
      if (url) return `Opened ${url}`;
      // codex reports page visits as an opaque {type:"other"} action —
      // "searched" would be wrong, all we know is the web tool ran.
      if (input.type === "other") return "Browsed the web";
      return desc ?? "Searched the web";
    }
    case "WebFetch":
      if (typeof input.url === "string") return `Fetched ${input.url}`;
      return desc ?? "Fetched a page";
    case "Task":
      return desc ?? "Ran a subagent";
    case "subagent":
      return subagentLine(input);
    case "error":
      return "Error";
    default: {
      const detail = desc ?? fp ?? cmd ?? part.state?.title ?? "";
      return detail ? `${tool}: ${detail}` : tool;
    }
  }
}

/** Readable one-liner for a Codex sub-agent spawn/activity row, from the
 * collab item fields the backend put in `state.input`. */
function subagentLine(input: Record<string, unknown>): string {
  const trim = (s: string) => (s.length > 60 ? `${s.slice(0, 60)}…` : s);
  const prompt = typeof input.prompt === "string" && input.prompt ? ` — “${trim(input.prompt)}”` : "";
  // collabAgentToolCall carries `tool`; subAgentActivity carries `kind`.
  switch (typeof input.tool === "string" ? input.tool : "") {
    case "spawnAgent":
      return `Spawned agent${prompt}`;
    case "sendInput":
      return `Sent input to agent${prompt}`;
    case "resumeAgent":
      return "Resumed agent";
    case "wait":
      return "Waiting on agent";
    case "closeAgent":
      return "Closed agent";
  }
  switch (typeof input.kind === "string" ? input.kind : "") {
    case "started":
      return "Sub-agent started";
    case "interacted":
      return "Sub-agent activity";
    case "interrupted":
      return "Sub-agent interrupted";
  }
  return "Sub-agent";
}

/** One expandable tool row inside a group: gray summary line, click to reveal
 * the input + output. */
function ToolRow({ part, onOpenFile }: { part: ChatPart; onOpenFile?: (path: string) => void }) {
  const state = part.state;
  const output = state?.error || state?.output || "";
  const cmd = typeof state?.input?.command === "string" ? state.input.command : null;
  const filePath = typeof state?.input?.filePath === "string" ? state.input.filePath : null;
  const hasDetail = Boolean(output || cmd || filePath);
  return (
    <details className="tool-row" open={false}>
      <summary>
        <span className={toolStatusClass(state?.status)} />
        <span className="tool-line">{toolLine(part)}</span>
        {filePath && onOpenFile && (
          <button
            className="tool-open file-link"
            title={`Open ${filePath}`}
            onClick={(e) => {
              e.preventDefault();
              e.stopPropagation();
              onOpenFile(filePath);
            }}
          >
            open
          </button>
        )}
      </summary>
      {hasDetail && (
        <div className="tool-detail">
          {cmd && <div className="tool-cmd-full">{cmd}</div>}
          {/* Safety net for pre-cap stored transcripts; the backend caps live
              tool output at 16k (TOOL_TEXT_CAP), so this slice must stay
              above that or it clips the truncation marker. */}
          {output && <div className="tool-output">{output.slice(0, 20000)}</div>}
        </div>
      )}
    </details>
  );
}

/** A run of consecutive tool calls, collapsed into one gray line like the
 * Claude desktop app ("Read hello.py" for one, "Used N tools" for several).
 * Clicking expands every row; a still-running tool auto-expands. */
function ToolGroup({ parts, onOpenFile }: { parts: ChatPart[]; onOpenFile?: (path: string) => void }) {
  const running = parts.some((p) => p.state?.status === "running");
  const errored = parts.some((p) => p.state?.status === "error");
  const [open, setOpen] = useState(false);
  // While a tool is in flight, show it live; collapse once the run settles.
  const expanded = open || running;

  const summary =
    parts.length === 1
      ? toolLine(parts[0])
      : running
        ? toolLine(parts.find((p) => p.state?.status === "running") ?? parts[parts.length - 1])
        : `Used ${parts.length} tools`;

  return (
    <div className={`tool-group ${errored ? "has-error" : ""}`}>
      <button className="tool-group-summary" onClick={() => setOpen((v) => !v)}>
        <span className={toolStatusClass(running ? "running" : errored ? "error" : "completed")} />
        <span className="tool-line">{summary}</span>
        <ChevronRight size={12} className={`tool-chevron ${expanded ? "open" : ""}`} />
      </button>
      {expanded && (
        <div className="tool-group-rows">
          {parts.map((p) => (
            <ToolRow key={p.id} part={p} onOpenFile={onOpenFile} />
          ))}
        </div>
      )}
    </div>
  );
}

/** Interactive card for a plan / permission / question prompt. Approving (or
 * answering) resumes the session. Once resolved, cards mirror Claude Code:
 * a permission leaves no trace, a plan collapses to an expandable
 * "Proposed plan" row, a question collapses to a compact record of the
 * chosen answer — all inline at the card's chronological position. */
function PromptCard({
  part,
  onRespond,
  onOpenFile,
  onOpenPlan,
}: {
  part: ChatPart;
  onRespond?: (answer: PromptAnswer) => void;
  onOpenFile?: (path: string) => void;
  onOpenPlan?: (plan: string, promptId: string) => void;
}) {
  const p = part.prompt as ChatPrompt;
  const [picked, setPicked] = useState<string[]>([]);
  // Read-only host (no onRespond): actions disabled or hidden, card visible.
  const done = !onRespond;

  const respond = (answer: Omit<PromptAnswer, "promptId">) =>
    onRespond?.({ promptId: part.id, ...answer });

  // Resolved rendering, keyed off `resolved` alone (`done` also covers
  // read-only hosts, where an *unresolved* card must stay visible).
  if (p.resolved) {
    if (p.kind === "permission") return null;
    if (p.kind === "plan") {
      // No echo (`approved` absent — stale-card cleanup, pre-echo history):
      // neutral "Resolved", not a checkmark implying approval. A denial with
      // a note asked for changes; without one it was a plain rejection.
      const outcome =
        p.approved === true
          ? "Plan approved"
          : p.approved === false
            ? p.note
              ? "Revision requested"
              : "Rejected"
            : "Resolved";
      const outcomeClass =
        p.approved === true
          ? "approved"
          : p.approved === false
            ? p.note
              ? "revised"
              : "rejected"
            : "";
      return (
        <details className="prompt-collapsed">
          <summary>
            <span className="prompt-collapsed-title">
              {p.synthesized ? "Plan" : "Proposed plan"}
            </span>
            <span className={`prompt-outcome ${outcomeClass}`}>{outcome}</span>
          </summary>
          <div className="prompt-collapsed-body">
            <Md text={p.plan ?? ""} onOpenFile={onOpenFile} />
            {p.note && <div className="prompt-collapsed-note">{p.note}</div>}
          </div>
        </details>
      );
    }
    // question — one line: header/question + what was chosen (or the typed
    // custom answer). No echo at all (stale-resolved): neutral "Resolved",
    // matching the plan row.
    const chosen = (p.answers ?? []).join(", ") || p.note || "";
    return (
      <details className="prompt-collapsed">
        <summary>
          <span className="prompt-collapsed-title">{p.header || p.question || "Question"}</span>
          <span className={`prompt-outcome ${chosen ? "chosen" : ""}`}>{chosen || "Resolved"}</span>
        </summary>
        <div className="prompt-collapsed-body">
          {/* The summary title already shows the question when there's no header. */}
          {p.header && p.question && <div className="prompt-q">{p.question}</div>}
          {(p.options ?? []).length > 0 && (
            <ul className="prompt-collapsed-options">
              {(p.options ?? []).map((o) => (
                <li key={o.label} className={p.answers?.includes(o.label) ? "sel" : ""}>
                  {o.label}
                </li>
              ))}
            </ul>
          )}
          {/* A note-only answer is already the summary outcome — don't echo it twice. */}
          {p.note && p.note !== chosen && <div className="prompt-collapsed-note">{p.note}</div>}
        </div>
      </details>
    );
  }

  if (p.kind === "plan") {
    // With a plan-strip host (onOpenPlan), the docked strip owns the approval
    // actions and the full plan lives in the right pane — the inline card is a
    // compact, clamped in-transcript record. Without one, it keeps its own
    // buttons (approving leaves plan mode; resumeMode values are
    // harness-agnostic permission-mode wire ids).
    const docked = !!onOpenPlan;
    return (
      <div className={`prompt-card plan ${done ? "readonly" : ""}`}>
        <div className="prompt-head">
          {p.synthesized ? "Plan mode — ready to proceed?" : "Proposed plan"}
        </div>
        <div className={`prompt-plan ${docked ? "clamped" : ""}`}>
          <Md text={p.plan ?? ""} onOpenFile={onOpenFile} />
        </div>
        {docked && (
          <button className="prompt-plan-open" onClick={() => onOpenPlan(p.plan ?? "", part.id)}>
            View full plan
          </button>
        )}
        {/* Strip-less fallback (unreachable in the main app — App always
            provides onOpenPlan): same action semantics as the strip. */}
        {!done && !docked && (
          <div className="prompt-actions">
            <button className="btn-primary" onClick={() => respond({ approve: true, resumeMode: "auto" })}>
              Accept and auto mode
            </button>
            <button className="btn-ghost" onClick={() => respond({ approve: true, resumeMode: "bypass" })}>
              Accept and bypass all
            </button>
            <button className="btn-ghost" onClick={() => respond({ approve: false })}>
              Reject
            </button>
          </div>
        )}
      </div>
    );
  }

  if (p.kind === "permission") {
    const summary =
      (typeof p.toolInput?.command === "string" && p.toolInput.command) ||
      (typeof p.toolInput?.filePath === "string" && p.toolInput.filePath) ||
      "";
    // Codex approval cards ship a human-readable reason (and fileChange cards
    // carry nothing else) — show it so the user knows what they're granting.
    const reason =
      (typeof p.toolInput?.reason === "string" && p.toolInput.reason) || "";
    return (
      <div className={`prompt-card permission ${done ? "readonly" : ""}`}>
        <div className="prompt-head">
          Permission needed: <code>{p.tool}</code>
        </div>
        {summary && <div className="prompt-sub">{summary}</div>}
        {reason && <div className="prompt-sub">{reason}</div>}
        {!done && (
          // No resumeMode: the harness picks the right one for an approval.
          // Claude resumes under `bypass` (the only mode that actually grants a
          // blocked tool — acceptEdits would re-deny Bash); inline harnesses
          // (opencode) reply once/reject keyed off `approve`. Deny denies either way.
          <div className="prompt-actions">
            <button className="btn-primary" onClick={() => respond({ approve: true })}>
              Allow
            </button>
            <button className="btn-ghost" onClick={() => respond({ approve: false })}>
              Deny
            </button>
          </div>
        )}
      </div>
    );
  }

  // question
  const toggle = (label: string) =>
    setPicked((cur) =>
      p.multiSelect
        ? cur.includes(label)
          ? cur.filter((l) => l !== label)
          : [...cur, label]
        : [label],
    );
  return (
    <div className={`prompt-card question ${done ? "readonly" : ""}`}>
      {p.header && <div className="prompt-head">{p.header}</div>}
      {p.question && <div className="prompt-q">{p.question}</div>}
      <div className="prompt-options">
        {(p.options ?? []).map((o) => {
          const sel = picked.includes(o.label);
          return (
            <button
              key={o.label}
              className={`prompt-option ${sel ? "sel" : ""}`}
              disabled={done}
              onClick={() => (done ? undefined : p.multiSelect ? toggle(o.label) : respond({ answers: [o.label] }))}
            >
              <span className="prompt-option-label">{o.label}</span>
              {o.description && <span className="prompt-option-desc">{o.description}</span>}
            </button>
          );
        })}
      </div>
      {p.multiSelect && !done && (
        <div className="prompt-actions">
          <button
            className="btn-primary"
            disabled={picked.length === 0}
            onClick={() => respond({ answers: picked })}
          >
            Submit
          </button>
        </div>
      )}
    </div>
  );
}

/** Whether a message renders anything once resolved-permission cards vanish —
 * a bridge permission card rides its own message, so resolving it leaves the
 * message empty and it must drop out of the transcript entirely. */
function messageHasVisibleContent(m: ChatMessage): boolean {
  if (m.role === "user") return true;
  return m.parts.some((part) => {
    if (part.type === "prompt")
      return !!part.prompt && !(part.prompt.resolved && part.prompt.kind === "permission");
    if (part.type === "text" || part.type === "reasoning") return !!part.text;
    return true; // tool, image, …
  });
}

/** Memoized: streaming re-broadcasts the whole updated message ~7x/sec, and
 * `upsertMessage` preserves object identity for every untouched message — so
 * only the message actually being streamed re-renders (and re-parses its
 * markdown/KaTeX), not the entire transcript. Callback props must stay
 * referentially stable for this to hold (see the useCallback/useMemo wiring
 * in ChatPanel). `Transcript` below adds a second boundary for the other hot
 * path — composer keystrokes re-render ChatPanel itself, and the transcript
 * memo stops those from touching the rows at all. */
const Message = memo(function Message({
  message,
  onOpenFile,
  onRespond,
  onOpenPlan,
  onOpenSubagent,
  skills,
}: {
  message: ChatMessage;
  onOpenFile?: (path: string) => void;
  onRespond?: (answer: PromptAnswer) => void;
  /** Open a plan's full markdown in the right pane (plan cards/strip). */
  onOpenPlan?: (plan: string, promptId: string) => void;
  /** Open a sub-agent's transcript in the right pane (spawn-row "view"). */
  onOpenSubagent?: (spawnPartId: string) => void;
  /** Known slash-skills, for rendering a leading `/name` as a command chip. */
  skills?: SkillInfo[];
}) {
  if (message.role === "user") {
    const text = message.parts
      .filter((p) => p.type === "text")
      .map((p) => p.text ?? "")
      .join("\n");
    // A leading known `/command` renders as the same chip the composer shows.
    // Unknown commands (or skills removed since) fall back to plain text.
    const slash = text.match(/^\/(\S+)([\s\S]*)$/);
    const command = slash ? skills?.find((s) => s.name === slash[1]) : undefined;
    // Optimistic parts carry a data URL; server parts carry a file name.
    const images = message.parts
      .filter((p) => p.type === "image" && p.text)
      .map((p) => (p.text!.startsWith("data:") ? p.text! : chatAttachmentUrl(p.text!)));
    return (
      <div className="msg-user">
        {command ? (
          <>
            <span className="skill-chip">/{command.name}</span>
            {slash![2]}
          </>
        ) : (
          text
        )}
        {images.length > 0 && (
          <div className="msg-images">
            {images.map((src, i) => (
              <a key={i} href={src} target="_blank" rel="noreferrer">
                <img src={src} alt="attachment" />
              </a>
            ))}
          </div>
        )}
      </div>
    );
  }
  return (
    <div className="msg-assistant">
      {renderParts(message.parts, { onOpenFile, onRespond, onOpenPlan, onOpenSubagent })}
    </div>
  );
});

/** Shared assistant-parts renderer, reused for a message body and (recursively)
 * for a sub-agent's nested transcript. Coalesces consecutive tool parts into one
 * collapsed group (Claude-desktop style); text / reasoning / prompt parts break
 * a run and render inline. A sub-agent spawn part (tool `subagent`) also breaks
 * the run and renders as its own nested block. */
function renderParts(
  parts: ChatPart[],
  opts: {
    onOpenFile?: (path: string) => void;
    onRespond?: (answer: PromptAnswer) => void;
    onOpenPlan?: (plan: string, promptId: string) => void;
    onOpenSubagent?: (spawnPartId: string) => void;
  },
): React.ReactNode[] {
  const { onOpenFile, onRespond, onOpenPlan, onOpenSubagent } = opts;
  const rendered: React.ReactNode[] = [];
  let toolRun: ChatPart[] = [];
  const flushTools = () => {
    if (toolRun.length === 0) return;
    rendered.push(
      <ToolGroup key={`tg-${toolRun[0].id}`} parts={toolRun} onOpenFile={onOpenFile} />,
    );
    toolRun = [];
  };
  for (const part of parts) {
    // A sub-agent spawn part streams its own transcript in `children` — render
    // it as a standalone nested block, not folded into a tool run.
    if (part.type === "tool" && part.tool === "subagent") {
      flushTools();
      rendered.push(
        <SubagentBlock
          key={part.id}
          part={part}
          onOpenFile={onOpenFile}
          onOpenPlan={onOpenPlan}
          onOpenSubagent={onOpenSubagent}
        />,
      );
      continue;
    }
    if (part.type === "tool") {
      toolRun.push(part);
      continue;
    }
    flushTools();
    if (part.type === "text" && part.text)
      rendered.push(<Md key={part.id} text={part.text} onOpenFile={onOpenFile} />);
    else if (part.type === "reasoning" && part.text)
      rendered.push(
        <details key={part.id} className="reasoning">
          <summary>thinking…</summary>
          {part.text}
        </details>,
      );
    else if (part.type === "prompt" && part.prompt)
      rendered.push(
        <PromptCard
          key={part.id}
          part={part}
          onRespond={onRespond}
          onOpenFile={onOpenFile}
          onOpenPlan={onOpenPlan}
        />,
      );
  }
  flushTools();
  return rendered;
}

/** Find a part by id anywhere in a parts tree (depth-first). Used by the
 * right-pane sub-agent tab to locate a spawn part across a session's messages. */
export function findPartById(parts: ChatPart[], id: string): ChatPart | null {
  for (const part of parts) {
    if (part.id === id) return part;
    const nested = part.children && findPartById(part.children, id);
    if (nested) return nested;
  }
  return null;
}

/** The sub-agent's transcript, rendered standalone in the right-pane tab. Reuses
 * `renderParts`, so it streams and nests exactly like the inline block. */
export function SubagentTranscript({
  parts,
  onOpenFile,
  onOpenSubagent,
}: {
  parts: ChatPart[];
  onOpenFile?: (path: string) => void;
  onOpenSubagent?: (spawnPartId: string) => void;
}) {
  return (
    <div className="msg-assistant">
      {parts.length === 0 ? (
        <div className="subagent-empty">No activity yet…</div>
      ) : (
        renderParts(parts, { onOpenFile, onOpenSubagent })
      )}
    </div>
  );
}

/** A Codex sub-agent spawn row + its streaming nested transcript. Reads like a
 * spawn header ("Spawned agent — …") with a status dot; expands to reveal the
 * sub-agent's own parts (rendered by the same `renderParts`, so nested
 * sub-agents recurse). Auto-expands while the sub-agent is running. */
function SubagentBlock({
  part,
  onOpenFile,
  onOpenPlan,
  onOpenSubagent,
}: {
  part: ChatPart;
  onOpenFile?: (path: string) => void;
  onOpenPlan?: (plan: string, promptId: string) => void;
  onOpenSubagent?: (spawnPartId: string) => void;
}) {
  const running = part.state?.status === "running";
  const errored = part.state?.status === "error";
  const children = part.children ?? [];
  const [open, setOpen] = useState(false);
  const expanded = open || running;
  return (
    <div className={`subagent-block ${errored ? "has-error" : ""}`}>
      <button className="subagent-summary" onClick={() => setOpen((v) => !v)}>
        <Users size={12} className="subagent-icon" />
        <span className={toolStatusClass(part.state?.status)} />
        <span className="tool-line">{toolLine(part)}</span>
        {onOpenSubagent && children.length > 0 && (
          <button
            className="tool-open"
            title="Open sub-agent transcript"
            onClick={(e) => {
              e.preventDefault();
              e.stopPropagation();
              onOpenSubagent(part.id);
            }}
          >
            view
          </button>
        )}
        <ChevronRight size={12} className={`tool-chevron ${expanded ? "open" : ""}`} />
      </button>
      {expanded && (
        <div className="subagent-transcript">
          {children.length === 0 ? (
            <div className="subagent-empty">{running ? "Working…" : "No activity"}</div>
          ) : (
            renderParts(children, { onOpenFile, onOpenPlan, onOpenSubagent })
          )}
        </div>
      )}
    </div>
  );
}

/** Memoized transcript: composer keystrokes re-render ChatPanel (draft state
 * lives there), and this boundary keeps them from re-allocating N Message
 * elements and running N memo comparisons. Every prop passed here must stay
 * referentially stable across keystrokes (memoized/useCallback, never inline)
 * or the boundary silently breaks — with that held, typing costs one shallow
 * compare instead of O(messages) work. */
const Transcript = memo(function Transcript({
  messages,
  onOpenFile,
  onRespond,
  onOpenPlan,
  onOpenSubagent,
  skills,
}: {
  messages: ChatMessage[];
  onOpenFile?: (path: string) => void;
  onRespond?: (answer: PromptAnswer) => void;
  onOpenPlan?: (plan: string, promptId: string) => void;
  onOpenSubagent?: (spawnPartId: string) => void;
  skills?: SkillInfo[];
}) {
  return (
    <>
      {messages.filter(messageHasVisibleContent).map((m) => (
        <Message
          key={m.id}
          message={m}
          onOpenFile={onOpenFile}
          onRespond={onRespond}
          onOpenPlan={onOpenPlan}
          onOpenSubagent={onOpenSubagent}
          skills={skills}
        />
      ))}
    </>
  );
});

// --- session rail ------------------------------------------------------------

type SessionFilter = "active" | "archived" | "all";

/** Menu label + rail section heading per filter — "Recents" for the default view. */
const SESSION_FILTERS: { id: SessionFilter; label: string; railLabel: string }[] = [
  { id: "active", label: "Active", railLabel: "Recents" },
  { id: "archived", label: "Archived", railLabel: "Archived" },
  { id: "all", label: "All", railLabel: "All sessions" },
];

/** Filter control beside the "Recents" label: Active (default) / Archived / All. */
function SessionFilterMenu({
  value,
  onChange,
}: {
  value: SessionFilter;
  onChange: (next: SessionFilter) => void;
}) {
  const { open, setOpen, ref } = usePopover();
  return (
    <div className="rail-filter" ref={ref}>
      <button
        className={`icon-btn rail-filter-btn ${value !== "active" ? "active" : ""}`}
        title="Filter sessions"
        aria-label="Filter sessions"
        onClick={() => setOpen((v) => !v)}
      >
        <SlidersHorizontal size={13} />
      </button>
      {open && (
        <div className="option-menu drop-down align-right">
          {SESSION_FILTERS.map((f) => (
            <button
              key={f.id}
              className="model-item"
              onClick={() => {
                onChange(f.id);
                setOpen(false);
              }}
            >
              <span>{f.label}</span>
              {value === f.id && <Check size={13} />}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

/** One Recents row. Hover swaps the timestamp for a three-dot menu with
 * Rename, Archive/Unarchive, and Delete (Claude-desktop style). Rename turns
 * the title into an inline input. */
function SessionRow({
  session,
  active,
  busy,
  waiting,
  onOpen,
  onRename,
  onSetArchived,
  onDelete,
}: {
  session: ChatSession;
  active: boolean;
  busy: boolean;
  /** Turn held on an unanswered card: steady dot, not the working pulse. */
  waiting: boolean;
  onOpen: () => void;
  onRename: (title: string) => void;
  onSetArchived: (archived: boolean) => void;
  onDelete: () => void;
}) {
  const { open, setOpen, ref } = usePopover();
  const title = session.title?.trim() || "Untitled";
  const [editing, setEditing] = useState(false);
  // Seeded by startEditing() before the input mounts; "" is just a placeholder.
  const [draft, setDraft] = useState("");
  const inputRef = useRef<HTMLInputElement>(null);

  function startEditing() {
    setDraft(session.title?.trim() || "");
    setEditing(true);
  }
  function commit() {
    const next = draft.trim();
    setEditing(false);
    // Only persist a real change; an empty title would be rejected server-side.
    if (next && next !== (session.title?.trim() || "")) onRename(next);
  }

  // Focus + select the input once the row enters edit mode.
  useEffect(() => {
    if (editing) {
      inputRef.current?.focus();
      inputRef.current?.select();
    }
  }, [editing]);

  // Not a <button>: the kebab is a real button and can't nest inside one.
  return (
    <div
      ref={ref}
      role="button"
      tabIndex={0}
      className={`session-row ${active ? "active" : ""} ${open ? "menu-open" : ""} ${
        editing ? "editing" : ""
      }`}
      title={`${HARNESS_LABELS[session.harness]}${session.model ? ` · ${session.model}` : ""}`}
      onClick={() => {
        // While editing, a body click is a no-op; blur/Enter/Esc drive it.
        if (editing) return;
        // With the menu open, a body click just dismisses it — switching
        // sessions underneath an open menu would leave it orphaned.
        if (open) setOpen(false);
        else onOpen();
      }}
      onKeyDown={(e) => {
        // Only keys aimed at the row itself: the kebab, menu items, and the
        // rename input are descendants, and preventDefault here would cancel
        // their activation.
        if (e.target !== e.currentTarget) return;
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          // Mirror the click branch: dismiss an open menu instead of
          // navigating underneath it.
          if (open) setOpen(false);
          else onOpen();
        }
      }}
    >
      <span className="session-dot">
        {busy && <span className={`busy-dot ${waiting ? "waiting" : ""}`} />}
      </span>
      {editing ? (
        <input
          ref={inputRef}
          className="session-title-input"
          aria-label="Session title"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onClick={(e) => e.stopPropagation()}
          onBlur={commit}
          onKeyDown={(e) => {
            e.stopPropagation();
            if (e.key === "Enter") {
              e.preventDefault();
              commit();
            } else if (e.key === "Escape") {
              e.preventDefault();
              setEditing(false);
            }
          }}
        />
      ) : (
        <span className="session-title">{title}</span>
      )}
      <span className="session-time">{relTime(session.updatedAt)}</span>
      <button
        className="session-menu-btn"
        title="Session options"
        aria-label="Session options"
        onClick={(e) => {
          e.stopPropagation();
          setOpen((v) => !v);
        }}
      >
        <MoreHorizontal size={14} />
      </button>
      {open && (
        <div className="option-menu drop-down session-menu">
          <button
            className="model-item"
            onClick={(e) => {
              e.stopPropagation();
              setOpen(false);
              startEditing();
            }}
          >
            <span>Rename</span>
          </button>
          <button
            className="model-item"
            onClick={(e) => {
              e.stopPropagation();
              setOpen(false);
              onSetArchived(!session.archived);
            }}
          >
            <span>{session.archived ? "Unarchive" : "Archive"}</span>
          </button>
          <button
            className="model-item danger"
            onClick={(e) => {
              e.stopPropagation();
              setOpen(false);
              onDelete();
            }}
          >
            <span>Delete</span>
          </button>
        </div>
      )}
    </div>
  );
}

// --- panel -------------------------------------------------------------------

export function ChatPanel({
  projectId,
  paperId,
  railHeader,
  railOpen,
  onShowRail,
  mainView,
  onSelectMainView,
  panelOpen,
  onTogglePanel,
  onOpenFile,
  onOpenPlan,
  onOpenSubagent,
  onOpenWorktree,
  onStartTour,
  children,
}: {
  projectId: string;
  /** arXiv id the project starts from — surfaces a /reproduce-paper shortcut. */
  paperId?: string | null;
  /** Back-to-projects + project name block rendered at the top of the rail. */
  railHeader?: React.ReactNode;
  /** Whether the agents rail is showing (collapsed via its own header icon). */
  railOpen: boolean;
  /** Reopen the rail (from the chat header's sidebar icon). */
  onShowRail: () => void;
  /** What the middle pane shows: chat, files, or a settings section. */
  mainView: "chat" | "files" | SettingsTab;
  onSelectMainView: (view: "chat" | "files" | SettingsTab) => void;
  /** Whether the right panel is showing (toggled from the chat header). */
  panelOpen: boolean;
  onTogglePanel: () => void;
  /** Open a project file in the right pane (chat tool rows are clickable).
   * `sessionId` is the chat session the click came from, so relative paths
   * can resolve against that session's worktree. */
  onOpenFile?: (path: string, sessionId?: string) => void;
  /** Open a plan's markdown as a right-pane tab (plan strip / plan cards). */
  onOpenPlan?: (plan: string, sessionId: string, promptId: string) => void;
  /** Open a sub-agent's transcript as a right-pane tab (spawn-row "view").
   * `sessionId` is the chat session; `spawnPartId` locates the spawn part. */
  onOpenSubagent?: (sessionId: string, spawnPartId: string) => void;
  /** Open the live worktree tab for a session (chat header worktree button). */
  onOpenWorktree?: (sessionId: string) => void;
  /** Replay the onboarding tour (chat header help button). */
  onStartTour?: () => void;
  /** Middle-pane content when a settings section is active (the SettingsView). */
  children?: React.ReactNode;
}) {
  const [sessions, setSessions] = useState<ChatSession[]>([]);
  const [activeId, setActiveId] = useState<string | null>(null);
  const [sessionFilter, setSessionFilter] = useState<SessionFilter>("active");
  const [draft, setDraft] = useState("");
  // Pasted/dropped images waiting in the composer, as data URLs.
  const [attachments, setAttachments] = useState<{ dataUrl: string; mediaType: string }[]>([]);
  const [state, dispatch] = useReducer(reducer, {
    messagesBySession: {},
    busySessions: new Set<string>(),
  });
  const [harnesses, setHarnesses] = useState<Harness[]>([]);
  const [selection, setSelection] = useState<ModelSelection | null>(loadSelection);
  // Unsent composer tweaks (model/mode/reasoning) for the *open* session — the
  // session's harness is fixed, so these override only its mutable settings and
  // are applied (and persisted server-side) on the next send. Cleared when the
  // active session changes. Distinct from `selection`, which is the sticky
  // global preference that seeds *new* sessions.
  const [sessionOverride, setSessionOverride] = useState<Partial<ModelSelection>>({});
  const loadedSessions = useRef(new Set<string>());
  // Tombstones: a turn finishing in the same instant as a delete can emit its
  // final chat.session upsert *after* chat.session.deleted; ignoring upserts
  // for known-deleted ids keeps the ghost row from coming back.
  const deletedIds = useRef(new Set<string>());
  const threadRef = useRef<HTMLDivElement>(null);
  const threadInnerRef = useRef<HTMLDivElement>(null);
  const stickToBottom = useRef(true);
  const composerRef = useRef<HTMLTextAreaElement>(null);

  // Slash-skills: menu state is derived from the draft — open while the first
  // token is an unfinished `/command` (no whitespace yet) with matches.
  const [skills, setSkills] = useState<SkillInfo[]>([]);
  const [skillIdx, setSkillIdx] = useState(0);
  const [skillMenuDismissed, setSkillMenuDismissed] = useState(false);
  // A picked skill renders as a chip on the textarea's first line
  // (Claude-desktop style); the textarea then holds only the args. send()
  // reassembles `/name args`, so the wire and transcript keep the plain-text
  // form. The chip overlays the textarea and the first line is indented past
  // it (text-indent), so long args wrap full-width beneath the chip instead
  // of being squeezed into a narrower column.
  const [pickedSkill, setPickedSkill] = useState<SkillInfo | null>(null);
  const chipRef = useRef<HTMLSpanElement>(null);
  const [chipIndent, setChipIndent] = useState(0);
  useLayoutEffect(() => {
    setChipIndent(pickedSkill && chipRef.current ? chipRef.current.offsetWidth + 8 : 0);
    syncChipScroll();
  }, [pickedSkill]);

  /** The chip belongs to the first line of *content*, so when the textarea
   * scrolls it must ride along (and clip at the wrapper) instead of sitting
   * fixed over whatever line scrolled to the top. */
  function syncChipScroll() {
    if (chipRef.current)
      chipRef.current.style.transform = `translateY(${-(composerRef.current?.scrollTop ?? 0)}px)`;
  }
  // IME guard: mid-composition text can transiently look like a full command.
  const composingRef = useRef(false);
  useEffect(() => {
    getSkills().then(setSkills).catch(() => {});
  }, []);
  const slashToken =
    !pickedSkill && draft.startsWith("/") && !/\s/.test(draft) ? draft.slice(1) : null;
  const skillMatches =
    slashToken !== null && !skillMenuDismissed
      ? skills.filter((s) => s.name.startsWith(slashToken.toLowerCase()))
      : [];
  const skillMenuOpen = skillMatches.length > 0;
  const activeSkillIdx = Math.min(skillIdx, Math.max(0, skillMatches.length - 1));
  useEffect(() => setSkillIdx(0), [slashToken]);

  function pickSkill(skill: SkillInfo) {
    setPickedSkill(skill);
    setDraft("");
    composerRef.current?.focus();
  }

  /** Backspace at the start deletes the command outright (Claude-desktop
   * behavior) — the args stay put; re-type `/` to pick another skill. */
  function removeSkillChip() {
    setPickedSkill(null);
    composerRef.current?.focus();
  }

  /** Queue image files (clipboard paste or drag-drop) as composer attachments. */
  function addImageFiles(files: File[]) {
    for (const file of files) {
      if (!/^image\/(png|jpeg|gif|webp)$/.test(file.type)) continue;
      const reader = new FileReader();
      reader.onload = () => {
        const dataUrl = reader.result as string;
        setAttachments((cur) => [...cur, { dataUrl, mediaType: file.type }]);
      };
      reader.readAsDataURL(file);
    }
  }

  function onComposerPaste(e: React.ClipboardEvent) {
    const files = Array.from(e.clipboardData.items)
      .filter((item) => item.kind === "file" && item.type.startsWith("image/"))
      .map((item) => item.getAsFile())
      .filter((f): f is File => f !== null);
    if (files.length > 0) {
      e.preventDefault();
      addImageFiles(files);
    }
  }

  // The open session, if any (its harness is locked; its model/mode/reasoning
  // are what the composer should reflect and edit).
  const openSession = sessions.find((s) => s.id === activeId);

  // The selection the composer displays and edits:
  //  * with a session open — that session's stored settings, with any unsent
  //    picker tweaks layered on. The harness is the session's, not the global.
  //  * with no session — the sticky global preference (seeds a new session).
  const composerSelection: ModelSelection | null = openSession
    ? {
        harness: openSession.harness,
        model: sessionOverride.model ?? openSession.model,
        permissionMode: sessionOverride.permissionMode ?? openSession.permissionMode,
        reasoningLevel: sessionOverride.reasoningLevel ?? openSession.reasoningLevel,
      }
    : (selection ?? defaultSelection(harnesses));
  const activeHarness = composerSelection
    ? harnesses.find((h) => h.id === composerSelection.harness)
    : undefined;
  const opts = activeHarness?.options;

  // Editing the pickers: a session-scoped tweak when a session is open (applied
  // on next send), else an update to the sticky global preference.
  const selectModel = (next: ModelSelection) => {
    if (openSession) {
      setSessionOverride({
        model: next.model,
        permissionMode: next.permissionMode,
        reasoningLevel: next.reasoningLevel,
      });
    } else {
      setSelection(next);
      localStorage.setItem(SELECTION_STORAGE_KEY, JSON.stringify(next));
    }
  };
  const setPermissionMode = (id: string) => {
    if (composerSelection) selectModel({ ...composerSelection, permissionMode: id });
  };
  const setReasoningLevel = (id: string) => {
    if (composerSelection) selectModel({ ...composerSelection, reasoningLevel: id });
  };

  // Reset everything when the project changes.
  useEffect(() => {
    setSessions([]);
    setActiveId(null);
    setDraft("");
    setPickedSkill(null);
    setAttachments([]);
    dispatch({ type: "reset" });
    loadedSessions.current = new Set();
    listChatSessions(projectId)
      .then((list) => {
        setSessions(list);
        // Prefer the newest non-archived session; archived ones stay hidden.
        setActiveId((cur) => cur ?? list.find((s) => !s.archived)?.id ?? null);
        dispatch({
          type: "seedBusy",
          sessions: list.filter((s) => s.busy).map((s) => s.id),
        });
      })
      .catch(() => {});
  }, [projectId]);

  // Load message history when a session becomes active.
  useEffect(() => {
    if (!activeId || loadedSessions.current.has(activeId)) return;
    loadedSessions.current.add(activeId);
    getChatMessages(activeId)
      .then((messages) => dispatch({ type: "seed", sessionId: activeId, messages }))
      .catch(() => loadedSessions.current.delete(activeId));
  }, [activeId]);

  // Chat events from the shared /api/events stream.
  useEffect(() => {
    return onChatEvent((ev) => {
      switch (ev.type) {
        case "session":
          if (ev.session.projectId !== projectId) return;
          if (deletedIds.current.has(ev.session.id)) return;
          setSessions((cur) => {
            const i = cur.findIndex((s) => s.id === ev.session.id);
            if (i < 0) return [ev.session, ...cur];
            const next = cur.slice();
            // An interrupted turn aborts before the persist block, so its
            // follow-up chat.session can lack usage the client already showed
            // live. Usage is never legitimately cleared, so keep the local
            // value whenever the incoming session omits one.
            next[i] = { ...ev.session, contextUsage: ev.session.contextUsage ?? cur[i].contextUsage };
            return next;
          });
          break;
        case "sessionDeleted":
          forgetSession(ev.sessionId);
          break;
        case "message":
          dispatch({ type: "upsertMessage", sessionId: ev.sessionId, message: ev.message });
          break;
        case "busy":
          dispatch({ type: "busy", sessionId: ev.sessionId, busy: ev.busy });
          break;
        case "usage":
          setSessions((cur) =>
            cur.map((s) => (s.id === ev.sessionId ? { ...s, contextUsage: ev.usage } : s)),
          );
          break;
      }
    });
  }, [projectId]);

  const messages = activeId ? (state.messagesBySession[activeId] ?? []) : [];
  const busy = activeId ? state.busySessions.has(activeId) : false;
  // A busy turn blocked on an unanswered HELD card (nativeId — a bridge or
  // inline mid-turn request) is waiting on the user, not the model. Drives
  // the status line and the rail dot (the composer button is keyed on
  // `pendingQuestion` instead — what send() can actually service). End-turn
  // cards (no nativeId) never coexist with a busy turn of their own, so
  // keying on nativeId avoids false positives from stale cards. (Sessions
  // whose transcripts aren't loaded fall back to plain busy.) Memoized so the
  // messages × parts scan stays off the per-keystroke render path.
  const waitingSessions = useMemo(() => {
    const waiting = new Set<string>();
    for (const id of state.busySessions) {
      if (
        (state.messagesBySession[id] ?? []).some((m) =>
          m.parts.some(
            (p) => p.type === "prompt" && p.prompt && !p.prompt.resolved && p.prompt.nativeId,
          ),
        )
      )
        waiting.add(id);
    }
    return waiting;
  }, [state.busySessions, state.messagesBySession]);
  const awaitingInput = activeId ? waitingSessions.has(activeId) : false;
  const activeSession = openSession;

  // The newest unresolved plan prompt, if any — it drives the docked strip
  // above the composer. Resolution re-emits the message over SSE, so this
  // recomputes to null and the strip disappears on its own.
  const pendingPlan = useMemo(() => {
    for (let i = messages.length - 1; i >= 0; i--) {
      for (const part of messages[i].parts) {
        if (part.type === "prompt" && part.prompt?.kind === "plan" && !part.prompt.resolved) {
          return {
            promptId: part.id,
            plan: part.prompt.plan ?? "",
            synthesized: !!part.prompt.synthesized,
          };
        }
      }
    }
    return null;
  }, [messages]);

  // The newest ANSWERABLE unresolved question card's part id: typed composer
  // text answers IT as a custom answer, instead of racing the held turn with
  // a new message (which the busy guard would reject/drop). Plan cards have
  // their own inline revise textarea (PlanStrip) and don't route through
  // here. Claude + Codex sessions: both accept a note-only reply (codex's
  // user_input_reply takes the note as the surfaced question's freeform
  // answer). Opencode is excluded — it rejects note-only replies (see
  // reply_inline), so its options stay the interface. A held (nativeId) card
  // is answerable only while its turn is alive — a zombie left by a process
  // restart must not capture the composer (its own buttons error and the
  // backend collapses it on the first attempt).
  const pendingQuestion = useMemo(() => {
    const harness = activeSession?.harness;
    if (!activeId || (harness !== "claude-code" && harness !== "codex")) return null;
    for (let i = messages.length - 1; i >= 0; i--) {
      for (const part of messages[i].parts) {
        if (part.type !== "prompt" || !part.prompt || part.prompt.resolved) continue;
        if (part.prompt.kind !== "question") continue;
        if (part.prompt.nativeId && !state.busySessions.has(activeId)) return null;
        return part.id;
      }
    }
    return null;
  }, [messages, activeSession?.harness, activeId, state.busySessions]);

  // A submitted plan revision, until its replacement card arrives: hides the
  // outgoing card's strip so it never sits there looking actionable while
  // the model rewrites the plan (the transcript's Working… spinner is the
  // feedback). Cleared when the session's turn ends or a DIFFERENT plan card
  // shows up in the same session — pendingPlan derives from the ACTIVE
  // session, so the replaced check must not fire on a session switch.
  const [revising, setRevising] = useState<{ sessionId: string; promptId: string } | null>(null);
  const revisingPlan = revising && revising.sessionId === activeId ? revising : null;
  useEffect(() => {
    if (!revising) return;
    const stillBusy = state.busySessions.has(revising.sessionId);
    const replaced =
      revising.sessionId === activeId && pendingPlan && pendingPlan.promptId !== revising.promptId;
    if (!stillBusy || replaced) setRevising(null);
  }, [revising, pendingPlan, state.busySessions, activeId]);

  // Plan opens are stamped with the session like file opens are. Memoized
  // (along with openFileInSession and respond below) so the memoized Message
  // rows don't all re-render on every streaming tick.
  const openPlan = useMemo(
    () =>
      onOpenPlan && activeId
        ? (plan: string, promptId: string) => onOpenPlan(plan, activeId, promptId)
        : undefined,
    [onOpenPlan, activeId],
  );

  const openSubagent = useMemo(
    () =>
      onOpenSubagent && activeId
        ? (spawnPartId: string) => onOpenSubagent(activeId, spawnPartId)
        : undefined,
    [onOpenSubagent, activeId],
  );

  // File opens resolve against the active session's worktree — the agent runs
  // there, so that's where its paths point.
  const openFileInSession = useMemo(
    () => onOpenFile && ((path: string) => onOpenFile(path, activeId ?? undefined)),
    [onOpenFile, activeId],
  );

  // Drop any unsent composer tweak when switching sessions, so it never bleeds
  // from one session's pickers onto another's.
  useEffect(() => setSessionOverride({}), [activeId]);

  // Opening a session — or remounting the thread (leaving a settings view,
  // history seeding in) — always starts pinned at the latest messages.
  const threadMounted = mainView === "chat" && (messages.length > 0 || busy);
  useLayoutEffect(() => {
    stickToBottom.current = true;
    const el = threadRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [activeId, threadMounted]);

  // Autoscroll while pinned. Layout effect, so history seeds and streamed
  // messages land already scrolled (no flash of the top of the thread).
  useLayoutEffect(() => {
    const el = threadRef.current;
    if (el && stickToBottom.current) el.scrollTop = el.scrollHeight;
  }, [messages, busy]);

  // Re-pin when the thread resizes without a message change — images loading,
  // tool rows expanding, the pane resizing.
  useEffect(() => {
    const el = threadRef.current;
    const inner = threadInnerRef.current;
    if (!el || !inner) return;
    const ro = new ResizeObserver(() => {
      if (stickToBottom.current) el.scrollTop = el.scrollHeight;
    });
    ro.observe(inner);
    ro.observe(el);
    return () => ro.disconnect();
  }, [threadMounted]);

  async function send() {
    const args = draft.trim();
    // Reassemble the picked skill chip into the plain `/name args` wire form —
    // the backend's slash expansion and the transcript both see only text.
    const text = pickedSkill ? `/${pickedSkill.name}${args ? ` ${args}` : ""}` : args;
    const pending = attachments;
    if (!text && pending.length === 0) return;
    // A pending question card owns plain typed text as a custom answer
    // (Claude-desktop behavior). This also works while the turn is HELD on
    // the card — where a new message would be rejected as busy and silently
    // dropped. A failed answer restores the draft so the text isn't lost.
    // (Auto-convert is off while a card is pending; a chip picked from the
    // menu or left over just serializes into the note text, same as typing it.)
    if (text && pendingQuestion && pending.length === 0) {
      setDraft("");
      setPickedSkill(null);
      void respond({ promptId: pendingQuestion, answers: [], note: text }).then((ok) => {
        if (!ok) setDraft((cur) => cur || text);
      });
      return;
    }
    if (busy) return;
    // `composerSelection` already resolves to the open session's settings (+ any
    // unsent tweak) or, for a new session, the global preference.
    const effective = composerSelection;
    if (!effective && !activeId) return; // no harness available at all
    setDraft("");
    setPickedSkill(null);
    setAttachments([]);
    let sid = activeId;
    try {
      if (!sid) {
        const session = await createChatSession(projectId, effective!.harness, {
          model: effective!.model,
          permissionMode: effective!.permissionMode,
          reasoningLevel: effective!.reasoningLevel,
        });
        loadedSessions.current.add(session.id);
        setSessions((cur) => [session, ...cur]);
        setActiveId(session.id);
        sid = session.id;
      }
      dispatch({
        type: "optimisticUser",
        sessionId: sid,
        text,
        imageUrls: pending.map((a) => a.dataUrl),
      });
      dispatch({ type: "busy", sessionId: sid, busy: true });
      stickToBottom.current = true;
      // The session being sent to is never archived after this turn (new ones
      // start active; existing ones are unarchived server-side by activity) —
      // leave the Archived-only view so its row stays visible.
      if (sessionFilter === "archived") setSessionFilter("active");
      // `effective.harness` is always the target session's harness (locked once
      // it exists), so these overrides are always valid — the backend persists
      // them as the session's sticky settings. Clear the unsent tweak now.
      const turnOpts = effective
        ? {
            model: effective.model,
            permissionMode: effective.permissionMode,
            reasoningLevel: effective.reasoningLevel,
          }
        : {};
      setSessionOverride({});
      const images: ChatImageAttachment[] = pending.map((a) => ({
        mediaType: a.mediaType,
        dataBase64: a.dataUrl.slice(a.dataUrl.indexOf(",") + 1),
      }));
      await sendChatMessage(sid, text, turnOpts, images.length ? images : undefined);
    } catch {
      if (sid) dispatch({ type: "busy", sessionId: sid, busy: false });
    }
  }

  function stop() {
    if (activeId) void interruptChat(activeId);
  }

  // Escape stops the streaming turn and drops focus back into the composer,
  // mirroring the Claude Code desktop app. Harness-agnostic — `stop()` →
  // `interruptChat` interrupts whichever harness (Claude, Codex, OpenCode, …)
  // is running the active session. Only armed on the chat view while busy, so
  // it never fires from the settings/files panels that also render inside
  // ChatPanel.
  //
  // An overlay that should swallow Escape (rather than let it stop the turn)
  // must own the key ahead of this document-level bubble listener, by one of
  // two means already in use — a new overlay has to pick one or it will
  // interrupt the turn on Escape:
  //   - the slash menu preventDefaults in the composer's onKeyDown (bubble),
  //     so the `defaultPrevented` guard below defers to it;
  //   - the composer pickers (usePopover) stopPropagation in the capture phase,
  //     so their Escape never reaches this listener at all.
  useEffect(() => {
    if (!busy || mainView !== "chat") return;
    function onKey(e: KeyboardEvent) {
      if (e.key !== "Escape" || e.defaultPrevented) return;
      e.preventDefault();
      stop();
      composerRef.current?.focus();
    }
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [busy, activeId, mainView]);

  /** Drop every trace of a session — the local row, the open-thread selection,
   * and the cached transcript. Used on delete (ours or another dashboard's). */
  function forgetSession(sessionId: string) {
    deletedIds.current.add(sessionId);
    setSessions((cur) => cur.filter((s) => s.id !== sessionId));
    setActiveId((cur) => (cur === sessionId ? null : cur));
    loadedSessions.current.delete(sessionId);
    dispatch({ type: "forget", sessionId });
  }

  function setArchived(session: ChatSession, archived: boolean) {
    // Optimistic; the server also broadcasts the row over chat.session. On
    // failure restore the pre-request snapshot (not the request's negation,
    // which could undo a concurrent authoritative update).
    const prev = session.archived;
    setSessions((cur) => cur.map((s) => (s.id === session.id ? { ...s, archived } : s)));
    void setChatSessionArchived(session.id, archived).catch(() => {
      setSessions((cur) =>
        cur.map((s) => (s.id === session.id ? { ...s, archived: prev } : s)),
      );
    });
  }

  function rename(session: ChatSession, title: string) {
    // Optimistic; the server trims and re-broadcasts the row over chat.session.
    // On failure restore the pre-request title (not the draft) so a concurrent
    // authoritative update isn't undone.
    const prev = session.title;
    setSessions((cur) => cur.map((s) => (s.id === session.id ? { ...s, title } : s)));
    void renameChatSession(session.id, title).catch(() => {
      setSessions((cur) => cur.map((s) => (s.id === session.id ? { ...s, title: prev } : s)));
    });
  }

  async function removeSession(session: ChatSession) {
    const title = session.title?.trim() || "Untitled";
    if (!window.confirm(`Delete "${title}"?\n\nIts transcript is permanently removed.`)) return;
    try {
      await deleteChatSession(session.id);
    } catch (err) {
      window.alert(
        `Failed to delete "${title}": ${err instanceof Error ? err.message : String(err)}`,
      );
      return;
    }
    forgetSession(session.id);
  }

  /** Deliver a card answer; resolves `false` when delivery failed (so a
   * caller can e.g. restore a consumed draft). Stable per session so the
   * memoized Message rows don't re-render on unrelated state changes. */
  const respond = useCallback(
    (answer: PromptAnswer): Promise<boolean> => {
      if (!activeId) return Promise.resolve(false);
      const sid = activeId;
      // The resumed turn streams over SSE; optimistically mark busy.
      dispatch({ type: "busy", sessionId: sid, busy: true });
      return respondChat(sid, answer)
        .then(() => true)
        .catch(() => false)
        .finally(() => {
          // Reconcile with the store: if this tab's copy of the card was stale
          // (e.g. the held turn timed out and resolved it while our SSE was
          // dropped), the answer no-ops server-side and nothing re-broadcasts —
          // without this the card stays actionable forever and every answer
          // silently dead-ends. Busy is reconciled from the server for THIS
          // session only (a whole-set replace could stomp another session's
          // just-started optimistic flag), so the optimistic dispatch above
          // can't wedge true after a no-op or failure.
          getChatMessages(sid)
            .then((messages) => dispatch({ type: "seed", sessionId: sid, messages }))
            .catch(() => {});
          listChatSessions(projectId)
            .then((list) =>
              dispatch({
                type: "busy",
                sessionId: sid,
                busy: !!list.find((s) => s.id === sid)?.busy,
              }),
            )
            // On a failed fetch keep the optimistic flag: clearing busy while a
            // Handled resume is still streaming would hide Working…/Stop for
            // the rest of the turn (nothing re-asserts busy mid-stream).
            .catch(() => {});
        });
    },
    [activeId, projectId],
  );

  const visibleSessions = sessions.filter((s) =>
    sessionFilter === "all" ? true : sessionFilter === "archived" ? s.archived : !s.archived,
  );

  const rail = (
    <aside className="session-rail floating-panel">
      {railHeader}
      {/* Top nav: new session + the settings sections (shown in the middle pane). */}
      <nav className="rail-nav">
        <button
          className="rail-nav-item"
          data-onboarding="new-session"
          onClick={() => {
            setActiveId(null);
            onSelectMainView("chat");
          }}
        >
          <Plus size={15} />
          New session
        </button>
        <button
          className={`rail-nav-item ${mainView === "files" ? "active" : ""}`}
          data-onboarding="nav-files"
          onClick={() => onSelectMainView("files")}
        >
          <FolderOpen size={15} />
          Files
        </button>
        {SETTINGS_NAV.map((item) => (
          <button
            key={item.id}
            className={`rail-nav-item ${mainView === item.id ? "active" : ""}`}
            data-onboarding={item.id === "compute" ? "nav-compute" : undefined}
            onClick={() => onSelectMainView(item.id)}
          >
            {item.icon}
            {item.label}
          </button>
        ))}
      </nav>
      <div className="rail-body">
        <div className="rail-section-head">
          <div className="rail-section-label">
            {SESSION_FILTERS.find((f) => f.id === sessionFilter)?.railLabel ?? "Recents"}
          </div>
          <SessionFilterMenu value={sessionFilter} onChange={setSessionFilter} />
        </div>
        {visibleSessions.map((s) => (
          <SessionRow
            key={s.id}
            session={s}
            active={s.id === activeId && mainView === "chat"}
            busy={state.busySessions.has(s.id)}
            waiting={waitingSessions.has(s.id)}
            onOpen={() => {
              setActiveId(s.id);
              onSelectMainView("chat");
            }}
            onRename={(title) => rename(s, title)}
            onSetArchived={(archived) => setArchived(s, archived)}
            onDelete={() => void removeSession(s)}
          />
        ))}
        {visibleSessions.length === 0 && (
          <div className="rail-empty">
            {sessionFilter === "archived"
              ? "No archived sessions"
              : sessions.length > 0
                ? "No active sessions"
                : "No sessions yet"}
          </div>
        )}
      </div>
    </aside>
  );

  // With the rail hidden, the header stretches to the full pane width
  // (Claude-desktop style): the reopen toggle sits in the window's top-left
  // corner with the title beside it, instead of riding the centered readable
  // column.
  const headerClass = `chat-header${railOpen ? "" : " rail-hidden"}`;
  const railReopen = !railOpen && (
    <button
      className="icon-btn"
      title="Show sidebar"
      aria-label="Show sidebar"
      onClick={onShowRail}
    >
      <PanelLeft size={15} />
    </button>
  );

  // A settings section replaces the chat entirely (no chat header, no
  // composer, no right panel) — only the rail-reopen affordance survives.
  // The pane spans the leftover width; .settings-view re-applies the readable
  // column from inside the scroller, same as .chat-thread-inner does for chat.
  if (mainView !== "chat") {
    return (
      <>
        {railOpen && rail}
        <section className="chat-pane">
          {!railOpen && <div className={headerClass}>{railReopen}</div>}
          <div className="settings-view-scroll">{children}</div>
        </section>
      </>
    );
  }

  return (
    <>
      {railOpen && rail}
      <section className="chat-pane">
      {/* Header — session title on the left, right-pane view switchers on the
          right, fading into the chat below (sessions live in the rail). */}
      <div className={headerClass}>
        {railReopen}
        <div
          className="title"
          title={activeSession ? activeSession.title?.trim() || "Untitled" : "New session"}
        >
          {activeSession ? activeSession.title?.trim() || "Untitled" : "New session"}
        </div>
        {onStartTour && (
          <button
            className="icon-btn"
            data-tip="Replay tour"
            aria-label="Replay tour"
            onClick={onStartTour}
          >
            <HelpCircle size={15} />
          </button>
        )}
        {onOpenWorktree && activeId && (
          <button
            className="icon-btn"
            data-tip="View session worktree"
            aria-label="View session worktree"
            onClick={() => onOpenWorktree(activeId)}
          >
            <FolderGit2 size={15} />
          </button>
        )}
        <button
          className={`icon-btn ${panelOpen ? "active" : ""}`}
          title="Experiments"
          aria-label="Experiments"
          onClick={onTogglePanel}
        >
          <FlaskConical size={15} />
        </button>
      </div>

      {!threadMounted ? (
        <div className="chat-empty">
          <h2>
            <Wordmark />
          </h2>
          <p>
            Ask the agent to explore your codebase, create and run your baseline experiment, and
            branch variants off it.
          </p>
          <EmptyStateAgentHint
            harnesses={harnesses}
            selection={selection ?? defaultSelection(harnesses)}
          />
          {paperId && (
            <button
              type="button"
              className="chat-suggest mono"
              title="Prefills the composer — add the compute to run on, then send"
              onClick={() => {
                const skill = skills.find((s) => s.name === "reproduce-paper");
                if (skill) {
                  setPickedSkill(skill);
                  setDraft(`${paperId} on `);
                } else {
                  // Skills fetch failed — plain text still expands server-side.
                  setDraft(`/reproduce-paper ${paperId} on `);
                }
                composerRef.current?.focus();
              }}
            >
              /reproduce-paper {paperId} on [describe your compute setup]
            </button>
          )}
        </div>
      ) : (
        <div
          className="chat-thread"
          ref={threadRef}
          onScroll={(e) => {
            const el = e.currentTarget;
            stickToBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < 60;
          }}
        >
          <div className="chat-thread-inner" ref={threadInnerRef}>
            <Transcript
              messages={messages}
              onOpenFile={openFileInSession}
              onRespond={respond}
              onOpenPlan={openPlan}
              onOpenSubagent={openSubagent}
              skills={skills}
            />
            {busy &&
              (awaitingInput ? (
                <div className="working awaiting">Waiting for your input…</div>
              ) : (
                <div className="working">
                  <span className="spinner" /> Working…
                </div>
              ))}
          </div>
        </div>
      )}

      {/* Docked while a plan awaits a decision, so the approval controls never
          scroll away. Actions mirror the (now compact) inline card's wire. */}
      <div className="composer">
        {/* Inside the composer so the composer's popovers (mode/model pickers,
            z 50 within this stacking context) layer above the strip — as a
            sibling, the composer's own z-index: 4 capped them below it. */}
        {/* Hidden while a submitted revision is in flight so the outgoing
            card never sits there looking actionable; the revised card swaps
            in when it arrives (effect above). The transcript status covers
            the interim ("Waiting for your input…" for a beat until the old
            card's resolve broadcast lands, then Working…). */}
        {pendingPlan && !(revisingPlan && pendingPlan.promptId === revisingPlan.promptId) && (
          <PlanStrip
            synthesized={pendingPlan.synthesized}
            agentLabel={
              activeSession ? HARNESS_LABELS[activeSession.harness] : "The agent"
            }
            onView={() => openPlan?.(pendingPlan.plan, pendingPlan.promptId)}
            onApprove={(resumeMode) =>
              respond({ promptId: pendingPlan.promptId, approve: true, resumeMode })
            }
            // Plain rejection — no note; the model stops and waits.
            onReject={() => respond({ promptId: pendingPlan.promptId, approve: false })}
            // The strip owns its own revise textarea (Claude-desktop style);
            // the note comes back on submit, always non-empty (note presence
            // is what distinguishes revise from reject on the wire).
            onRevise={(note) => {
              if (activeId) setRevising({ sessionId: activeId, promptId: pendingPlan.promptId });
              respond({ promptId: pendingPlan.promptId, approve: false, note });
            }}
          />
        )}
        <div className="composer-box" data-onboarding="composer">
          {skillMenuOpen && (
            <SkillMenu
              skills={skillMatches}
              activeIndex={activeSkillIdx}
              onPick={pickSkill}
              onHover={setSkillIdx}
            />
          )}
          {attachments.length > 0 && (
            <div className="composer-attachments">
              {attachments.map((a, i) => (
                <div key={i} className="attachment-thumb">
                  <img src={a.dataUrl} alt="pasted" />
                  <button
                    title="Remove image"
                    aria-label="Remove image"
                    onClick={() => setAttachments((cur) => cur.filter((_, j) => j !== i))}
                  >
                    <X size={11} />
                  </button>
                </div>
              ))}
            </div>
          )}
          <div className="composer-input">
            {pickedSkill && (
              // Inert like inline text: clicks fall through to the textarea
              // (pointer-events: none); Backspace at the start removes it.
              <span ref={chipRef} className="skill-chip composer-chip">
                /{pickedSkill.name}
              </span>
            )}
            <textarea
              ref={composerRef}
              value={draft}
              style={pickedSkill ? { textIndent: chipIndent } : undefined}
              onScroll={syncChipScroll}
              placeholder={
                // A pending question card owns typed text (see send()); say so.
                // With a chip active, the skill's arg hint says what to type —
                // and when the project already has a paper attached, the paper
                // part of the paper-reproduction skills defaults to it, so mark
                // just that part optional (compute is still expected).
                // Otherwise follow `composerSelection` so the name tracks the
                // picker for a new session and the open session once one exists.
                pendingQuestion
                  ? "Type a custom answer…"
                  : pickedSkill
                    ? ["reproduce-paper", "paper-to-marimo"].includes(pickedSkill.name) &&
                      paperId
                      ? `[paper — optional, defaults to ${paperId}] on [compute]`
                      : pickedSkill.argHint
                    : composerSelection
                      ? `Message ${HARNESS_LABELS[composerSelection.harness]}… ( / for skills)`
                      : "Ask the research agent… ( / for skills)"
              }
              rows={2}
              onPaste={onComposerPaste}
              onDragOver={(e) => {
                if (e.dataTransfer.types.includes("Files")) e.preventDefault();
              }}
              onDrop={(e) => {
                if (e.dataTransfer.files.length === 0) return;
                e.preventDefault();
                addImageFiles(Array.from(e.dataTransfer.files));
              }}
              onChange={(e) => {
                const v = e.target.value;
                // Auto-convert a typed/pasted full `/name ` into the chip the
                // moment the space lands. Known names only — unknown `/foo`
                // stays plain text (server-side pass-through contract). Not
                // while a question card is pending (its answer is a note, never
                // skill-expanded) and not mid-IME-composition.
                if (!pickedSkill && !pendingQuestion && !composingRef.current) {
                  const m = v.match(/^\/(\S+)\s([\s\S]*)$/);
                  const hit = m && skills.find((s) => s.name === m[1].toLowerCase());
                  if (hit) {
                    setPickedSkill(hit);
                    setDraft(m[2]);
                    setSkillMenuDismissed(false);
                    return;
                  }
                }
                setDraft(v);
                setSkillMenuDismissed(false);
              }}
              onCompositionStart={() => {
                composingRef.current = true;
              }}
              onCompositionEnd={() => {
                composingRef.current = false;
              }}
              onKeyDown={(e) => {
                if (skillMenuOpen) {
                  if (e.key === "ArrowDown" || e.key === "ArrowUp") {
                    e.preventDefault();
                    const delta = e.key === "ArrowDown" ? 1 : -1;
                    setSkillIdx(
                      (activeSkillIdx + delta + skillMatches.length) % skillMatches.length,
                    );
                    return;
                  }
                  if (e.key === "Enter" || e.key === "Tab") {
                    e.preventDefault();
                    pickSkill(skillMatches[activeSkillIdx]);
                    return;
                  }
                  if (e.key === "Escape") {
                    e.preventDefault();
                    setSkillMenuDismissed(true);
                    return;
                  }
                }
                // Backspace at the very start deletes the command chip.
                // (Escape deliberately doesn't touch the chip — it's the
                // stop-the-turn gesture, see the document listener above.)
                if (
                  pickedSkill &&
                  e.key === "Backspace" &&
                  e.currentTarget.selectionStart === 0 &&
                  e.currentTarget.selectionEnd === 0
                ) {
                  e.preventDefault();
                  removeSkillChip();
                  return;
                }
                if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
                  e.preventDefault();
                  void send();
                }
              }}
            />
          </div>
          <div className="composer-actions">
            {/* Bottom-left: permission mode. */}
            <OptionPicker
              choices={opts?.permissionModes ?? []}
              value={composerSelection?.permissionMode ?? null}
              defaultId={opts?.defaultPermissionMode ?? null}
              header="Mode"
              align="left"
              variant="pill"
              numbered
              title="Permission mode for this chat"
              onSelect={setPermissionMode}
            />
            <div style={{ flex: 1 }} />
            {/* Bottom-right: model, reasoning level, then context meter. The
                picker reflects the open session (harness locked once it exists);
                the global default only applies before the first message. */}
            <ModelPicker
              value={composerSelection}
              onSelect={selectModel}
              onHarnesses={setHarnesses}
              lockHarness={!!openSession}
            />
            <OptionPicker
              choices={opts?.reasoningLevels ?? []}
              value={composerSelection?.reasoningLevel ?? null}
              defaultId={opts?.defaultReasoningLevel ?? null}
              header="Reasoning"
              align="right"
              variant="bare"
              title="Reasoning level for this chat"
              onSelect={setReasoningLevel}
            />
            <ContextMeter usage={openSession?.contextUsage} />
            {busy && !pendingQuestion ? (
              // Stop whenever the turn is busy and typed text has nowhere to
              // go — actively streaming, or held on a plan/permission card
              // (their cards are the affordance; send() can't service them).
              // Send stays only when it actually works: idle, or a held
              // QUESTION card that owns typed text.
              <button className="send-btn stop" title="Stop" aria-label="Stop" onClick={stop}>
                <X size={16} />
              </button>
            ) : (
              <button
                className="send-btn"
                title="Send"
                aria-label="Send"
                onClick={() => void send()}
                disabled={!pickedSkill && !draft.trim() && attachments.length === 0}
              >
                <CornerDownLeft size={16} />
              </button>
            )}
          </div>
        </div>
      </div>
      </section>
    </>
  );
}
