import { ChevronRight, CornerDownLeft, FlaskConical, FolderOpen, PanelLeft, Plus, X } from "lucide-react";
import { useEffect, useReducer, useRef, useState } from "react";
import {
  chatAttachmentUrl,
  createChatSession,
  getChatMessages,
  getSkills,
  interruptChat,
  listChatSessions,
  respondChat,
  sendChatMessage,
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
import { SETTINGS_NAV, type SettingsTab } from "./SettingsPage";
import { SkillMenu } from "./SkillMenu";
import {
  defaultSelection,
  HARNESS_LABELS,
  ModelPicker,
  OptionPicker,
  type ModelSelection,
} from "./ModelPicker";

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
  | { type: "seedBusy"; sessions: string[] };

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
    case "WebFetch":
    case "WebSearch":
      return desc ?? "Searched the web";
    case "Task":
      return desc ?? "Ran a subagent";
    case "error":
      return "Error";
    default: {
      const detail = desc ?? fp ?? cmd ?? part.state?.title ?? "";
      return detail ? `${tool}: ${detail}` : tool;
    }
  }
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
 * answering) resumes the session; the card renders read-only once resolved. */
function PromptCard({
  part,
  onRespond,
  onOpenFile,
}: {
  part: ChatPart;
  onRespond?: (answer: PromptAnswer) => void;
  onOpenFile?: (path: string) => void;
}) {
  const p = part.prompt as ChatPrompt;
  const [picked, setPicked] = useState<string[]>([]);
  const done = p.resolved || !onRespond;

  const respond = (answer: Omit<PromptAnswer, "promptId">) =>
    onRespond?.({ promptId: part.id, ...answer });

  if (p.kind === "plan") {
    return (
      <div className={`prompt-card plan ${done ? "resolved" : ""}`}>
        <div className="prompt-head">Proposed plan</div>
        <div className="prompt-plan">
          <Md text={p.plan ?? ""} onOpenFile={onOpenFile} />
        </div>
        {done ? (
          <div className="prompt-resolved">Resolved</div>
        ) : (
          // Plan prompts are Claude-only. Approving leaves plan mode to actually
          // run the work, so the two modes offered are the ones headless honors
          // for that: Auto (balanced — runs tools) and Bypass (no sandbox).
          // resumeMode values are harness-agnostic permission-mode wire ids.
          <div className="prompt-actions">
            <button className="btn-primary" onClick={() => respond({ approve: true, resumeMode: "auto" })}>
              Approve &amp; run
            </button>
            <button className="btn-ghost" onClick={() => respond({ approve: true, resumeMode: "bypass" })}>
              Approve &amp; bypass all
            </button>
            <button className="btn-ghost" onClick={() => respond({ approve: false })}>
              Keep planning
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
    return (
      <div className={`prompt-card permission ${done ? "resolved" : ""}`}>
        <div className="prompt-head">
          Permission needed: <code>{p.tool}</code>
        </div>
        {summary && <div className="prompt-sub">{summary}</div>}
        {done ? (
          <div className="prompt-resolved">Resolved</div>
        ) : (
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
    <div className={`prompt-card question ${done ? "resolved" : ""}`}>
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
      {done && <div className="prompt-resolved">Resolved</div>}
    </div>
  );
}

function Message({
  message,
  onOpenFile,
  onRespond,
}: {
  message: ChatMessage;
  onOpenFile?: (path: string) => void;
  onRespond?: (answer: PromptAnswer) => void;
}) {
  if (message.role === "user") {
    const text = message.parts
      .filter((p) => p.type === "text")
      .map((p) => p.text ?? "")
      .join("\n");
    // Optimistic parts carry a data URL; server parts carry a file name.
    const images = message.parts
      .filter((p) => p.type === "image" && p.text)
      .map((p) => (p.text!.startsWith("data:") ? p.text! : chatAttachmentUrl(p.text!)));
    return (
      <div className="msg-user">
        {text}
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
  // Coalesce consecutive tool parts into one collapsed group (Claude-desktop
  // style); text / reasoning / prompt parts break a run and render inline.
  const rendered: React.ReactNode[] = [];
  let toolRun: ChatPart[] = [];
  const flushTools = () => {
    if (toolRun.length === 0) return;
    rendered.push(
      <ToolGroup key={`tg-${toolRun[0].id}`} parts={toolRun} onOpenFile={onOpenFile} />,
    );
    toolRun = [];
  };
  for (const part of message.parts) {
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
        <PromptCard key={part.id} part={part} onRespond={onRespond} onOpenFile={onOpenFile} />,
      );
  }
  flushTools();

  return <div className="msg-assistant">{rendered}</div>;
}

// --- panel -------------------------------------------------------------------

export function ChatPanel({
  projectId,
  railHeader,
  railOpen,
  onShowRail,
  mainView,
  onSelectMainView,
  panelOpen,
  onTogglePanel,
  onOpenFile,
  children,
}: {
  projectId: string;
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
  /** Open a project file in the right pane (chat tool rows are clickable). */
  onOpenFile?: (path: string) => void;
  /** Middle-pane content when a settings section is active (the SettingsView). */
  children?: React.ReactNode;
}) {
  const [sessions, setSessions] = useState<ChatSession[]>([]);
  const [activeId, setActiveId] = useState<string | null>(null);
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
  const threadRef = useRef<HTMLDivElement>(null);
  const stickToBottom = useRef(true);
  const composerRef = useRef<HTMLTextAreaElement>(null);

  // Slash-skills: menu state is derived from the draft — open while the first
  // token is an unfinished `/command` (no whitespace yet) with matches.
  const [skills, setSkills] = useState<SkillInfo[]>([]);
  const [skillIdx, setSkillIdx] = useState(0);
  const [skillMenuDismissed, setSkillMenuDismissed] = useState(false);
  useEffect(() => {
    getSkills().then(setSkills).catch(() => {});
  }, []);
  const slashToken = draft.startsWith("/") && !/\s/.test(draft) ? draft.slice(1) : null;
  const skillMatches =
    slashToken !== null && !skillMenuDismissed
      ? skills.filter((s) => s.name.startsWith(slashToken.toLowerCase()))
      : [];
  const skillMenuOpen = skillMatches.length > 0;
  const activeSkillIdx = Math.min(skillIdx, Math.max(0, skillMatches.length - 1));
  useEffect(() => setSkillIdx(0), [slashToken]);

  function pickSkill(skill: SkillInfo) {
    setDraft(`/${skill.name} `);
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
    setAttachments([]);
    dispatch({ type: "reset" });
    loadedSessions.current = new Set();
    listChatSessions(projectId)
      .then((list) => {
        setSessions(list);
        setActiveId((cur) => cur ?? list[0]?.id ?? null);
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
          setSessions((cur) => {
            const i = cur.findIndex((s) => s.id === ev.session.id);
            if (i < 0) return [ev.session, ...cur];
            const next = cur.slice();
            next[i] = ev.session;
            return next;
          });
          break;
        case "message":
          dispatch({ type: "upsertMessage", sessionId: ev.sessionId, message: ev.message });
          break;
        case "busy":
          dispatch({ type: "busy", sessionId: ev.sessionId, busy: ev.busy });
          break;
      }
    });
  }, [projectId]);

  const messages = activeId ? (state.messagesBySession[activeId] ?? []) : [];
  const busy = activeId ? state.busySessions.has(activeId) : false;
  const activeSession = openSession;

  // Drop any unsent composer tweak when switching sessions, so it never bleeds
  // from one session's pickers onto another's.
  useEffect(() => setSessionOverride({}), [activeId]);

  // Autoscroll while pinned to the bottom.
  useEffect(() => {
    const el = threadRef.current;
    if (el && stickToBottom.current) el.scrollTop = el.scrollHeight;
  }, [messages, busy]);

  async function send() {
    const text = draft.trim();
    const pending = attachments;
    if ((!text && pending.length === 0) || busy) return;
    // `composerSelection` already resolves to the open session's settings (+ any
    // unsent tweak) or, for a new session, the global preference.
    const effective = composerSelection;
    if (!effective && !activeId) return; // no harness available at all
    setDraft("");
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

  function respond(answer: PromptAnswer) {
    if (!activeId) return;
    // The resumed turn streams over SSE; optimistically mark busy.
    dispatch({ type: "busy", sessionId: activeId, busy: true });
    void respondChat(activeId, answer).catch(() => {
      if (activeId) dispatch({ type: "busy", sessionId: activeId, busy: false });
    });
  }

  const rail = (
    <aside className="session-rail floating-panel">
      {railHeader}
      {/* Top nav: new session + the settings sections (shown in the middle pane). */}
      <nav className="rail-nav">
        <button
          className="rail-nav-item"
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
          onClick={() => onSelectMainView("files")}
        >
          <FolderOpen size={15} />
          Files
        </button>
        {SETTINGS_NAV.map((item) => (
          <button
            key={item.id}
            className={`rail-nav-item ${mainView === item.id ? "active" : ""}`}
            onClick={() => onSelectMainView(item.id)}
          >
            {item.icon}
            {item.label}
          </button>
        ))}
      </nav>
      <div className="rail-body">
        <div className="rail-section-label">Recents</div>
        {sessions.map((s) => (
          <button
            key={s.id}
            className={`session-row ${s.id === activeId && mainView === "chat" ? "active" : ""}`}
            title={`${HARNESS_LABELS[s.harness]}${s.model ? ` · ${s.model}` : ""}`}
            onClick={() => {
              setActiveId(s.id);
              onSelectMainView("chat");
            }}
          >
            <span className="session-dot">
              {state.busySessions.has(s.id) && <span className="busy-dot" />}
            </span>
            <span className="session-title">{s.title?.trim() || "Untitled"}</span>
            <span className="session-time">{relTime(s.updatedAt)}</span>
          </button>
        ))}
      </div>
    </aside>
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
          {!railOpen && (
            <div className="chat-header">
              <button
                className="icon-btn"
                title="Show sidebar"
                aria-label="Show sidebar"
                onClick={onShowRail}
              >
                <PanelLeft size={15} />
              </button>
            </div>
          )}
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
      <div className="chat-header">
        {!railOpen && (
          <button
            className="icon-btn"
            title="Show sidebar"
            aria-label="Show sidebar"
            onClick={onShowRail}
          >
            <PanelLeft size={15} />
          </button>
        )}
        <div
          className="title"
          title={activeSession ? activeSession.title?.trim() || "Untitled" : "New session"}
        >
          {activeSession ? activeSession.title?.trim() || "Untitled" : "New session"}
        </div>
        <button
          className={`icon-btn ${panelOpen ? "active" : ""}`}
          title="Experiments"
          aria-label="Experiments"
          onClick={onTogglePanel}
        >
          <FlaskConical size={15} />
        </button>
      </div>

      {messages.length === 0 && !busy ? (
        <div className="chat-empty">
          <h2>
            Open<span>Research</span>
          </h2>
          <p>Ask the agent to explore your codebase, branch experiments, and launch runs.</p>
          <EmptyStateAgentHint
            harnesses={harnesses}
            selection={selection ?? defaultSelection(harnesses)}
          />
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
          <div className="chat-thread-inner">
            {messages.map((m) => (
              <Message key={m.id} message={m} onOpenFile={onOpenFile} onRespond={respond} />
            ))}
            {busy && (
              <div className="working">
                <span className="spinner" /> Working…
              </div>
            )}
          </div>
        </div>
      )}

      <div className="composer">
        <div className="composer-box">
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
          <textarea
            ref={composerRef}
            value={draft}
            placeholder={
              // Follow `composerSelection` so the name tracks the picker for a
              // new session and the open session once one exists.
              composerSelection
                ? `Message ${HARNESS_LABELS[composerSelection.harness]}…`
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
              setDraft(e.target.value);
              setSkillMenuDismissed(false);
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
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                void send();
              }
            }}
          />
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
            {/* Bottom-right: model, then reasoning level. The picker reflects the
                open session (harness locked once it exists); the global default
                only applies before the first message. */}
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
            {busy ? (
              <button className="send-btn stop" title="Stop" aria-label="Stop" onClick={stop}>
                <X size={16} />
              </button>
            ) : (
              <button
                className="send-btn"
                title="Send"
                aria-label="Send"
                onClick={() => void send()}
                disabled={!draft.trim() && attachments.length === 0}
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
