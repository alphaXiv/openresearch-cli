import { ArrowUp, Cpu, Plus, X } from "lucide-react";
import { useCallback, useEffect, useReducer, useRef, useState } from "react";
import {
  chatAttachmentUrl,
  createChatSession,
  deleteChatSession,
  getChatMessages,
  getSkills,
  interruptChat,
  listChatSessions,
  sendChatMessage,
  type ChatImageAttachment,
  type ChatMessage,
  type ChatPart,
  type ChatSession,
  type Harness,
  type SkillInfo,
} from "../api";
import { onChatEvent } from "../events";
import { Md } from "./Md";
import { ClosableTab } from "./ClosableTab";
import { SkillMenu } from "./SkillMenu";
import {
  defaultSelection,
  HARNESS_LABELS,
  ModelPicker,
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

function toolSummary(part: ChatPart): string {
  const input = part.state?.input;
  if (typeof input?.command === "string") return input.command;
  if (typeof input?.filePath === "string") return input.filePath;
  if (typeof input?.description === "string") return input.description;
  return part.state?.title ?? "";
}

function ToolRow({ part, onOpenFile }: { part: ChatPart; onOpenFile?: (path: string) => void }) {
  const state = part.state;
  const output = state?.error || state?.output || "";
  const filePath = typeof state?.input?.filePath === "string" ? state.input.filePath : null;
  return (
    <details className="tool-row">
      <summary>
        <span className={toolStatusClass(state?.status)} />
        <span className="tool-name">{part.tool ?? "tool"}</span>
        {filePath && onOpenFile ? (
          <button
            className="tool-cmd file-link"
            title={`Open ${filePath}`}
            onClick={(e) => {
              e.preventDefault(); // keep the <details> from toggling
              e.stopPropagation();
              onOpenFile(filePath);
            }}
          >
            {filePath}
          </button>
        ) : (
          <span className="tool-cmd">{toolSummary(part)}</span>
        )}
      </summary>
      {output ? <div className="tool-output">{output.slice(0, 20000)}</div> : null}
    </details>
  );
}

function Message({
  message,
  onOpenFile,
}: {
  message: ChatMessage;
  onOpenFile?: (path: string) => void;
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
        <span className="eyebrow">You</span>
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
  return (
    <div className="msg-assistant">
      {message.parts.map((part) => {
        if (part.type === "text" && part.text)
          return <Md key={part.id} text={part.text} onOpenFile={onOpenFile} />;
        if (part.type === "reasoning" && part.text)
          return (
            <details key={part.id} className="reasoning">
              <summary>thinking…</summary>
              {part.text}
            </details>
          );
        if (part.type === "tool")
          return <ToolRow key={part.id} part={part} onOpenFile={onOpenFile} />;
        return null;
      })}
    </div>
  );
}

// --- panel -------------------------------------------------------------------

export function ChatPanel({
  projectId,
  railHeader,
  onOpenFile,
  onOpenCompute,
}: {
  projectId: string;
  /** Brand + project switcher block rendered at the top of the agents rail. */
  railHeader?: React.ReactNode;
  /** Open a project file in the right pane (chat tool rows are clickable). */
  onOpenFile?: (path: string) => void;
  /** Open Settings with the Compute tab active. */
  onOpenCompute?: () => void;
}) {
  const [sessions, setSessions] = useState<ChatSession[]>([]);
  const [activeId, setActiveId] = useState<string | null>(null);
  // Sessions open as tabs in the chat header, in strip order. Selecting a
  // session (rail or strip) opens a tab; closing one only removes it here.
  const [openTabs, setOpenTabs] = useState<string[]>([]);
  const [draft, setDraft] = useState("");
  // Pasted/dropped images waiting in the composer, as data URLs.
  const [attachments, setAttachments] = useState<{ dataUrl: string; mediaType: string }[]>([]);
  const [state, dispatch] = useReducer(reducer, {
    messagesBySession: {},
    busySessions: new Set<string>(),
  });
  const [harnesses, setHarnesses] = useState<Harness[]>([]);
  const [selection, setSelection] = useState<ModelSelection | null>(loadSelection);
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

  const selectModel = (next: ModelSelection) => {
    setSelection(next);
    localStorage.setItem(SELECTION_STORAGE_KEY, JSON.stringify(next));
  };

  // Reset everything when the project changes.
  useEffect(() => {
    setSessions([]);
    setActiveId(null);
    setOpenTabs([]);
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

  // Whatever session becomes active always gets a tab — covers the initially
  // auto-selected session and drafts that materialize on first send.
  useEffect(() => {
    if (!activeId) return;
    setOpenTabs((prev) => (prev.includes(activeId) ? prev : [...prev, activeId]));
  }, [activeId]);

  const closeTab = useCallback(
    (id: string) => {
      const idx = openTabs.indexOf(id);
      const next = openTabs.filter((t) => t !== id);
      setOpenTabs(next);
      // Closing the active tab falls back to a neighbor, else the draft page.
      setActiveId((cur) => (cur === id ? (next[Math.min(idx, next.length - 1)] ?? null) : cur));
    },
    [openTabs],
  );

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
  const activeSession = sessions.find((s) => s.id === activeId);

  // Autoscroll while pinned to the bottom.
  useEffect(() => {
    const el = threadRef.current;
    if (el && stickToBottom.current) el.scrollTop = el.scrollHeight;
  }, [messages, busy]);

  async function send() {
    const text = draft.trim();
    const pending = attachments;
    if ((!text && pending.length === 0) || busy) return;
    const effective = selection ?? defaultSelection(harnesses);
    if (!effective && !activeId) return; // no harness available at all
    setDraft("");
    setAttachments([]);
    let sid = activeId;
    try {
      if (!sid) {
        const session = await createChatSession(
          projectId,
          effective!.harness,
          effective!.model,
        );
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
      const current = sessions.find((s) => s.id === sid);
      // Model overrides only apply within the session's own harness.
      const model =
        effective && (!current || current.harness === effective.harness)
          ? effective.model
          : undefined;
      const images: ChatImageAttachment[] = pending.map((a) => ({
        mediaType: a.mediaType,
        dataBase64: a.dataUrl.slice(a.dataUrl.indexOf(",") + 1),
      }));
      await sendChatMessage(sid, text, model, images.length ? images : undefined);
    } catch {
      if (sid) dispatch({ type: "busy", sessionId: sid, busy: false });
    }
  }

  function stop() {
    if (activeId) void interruptChat(activeId);
  }

  async function removeSession(id: string) {
    await deleteChatSession(id).catch(() => false);
    setSessions((cur) => cur.filter((s) => s.id !== id));
    closeTab(id);
  }

  const rail = (
    <aside className="session-rail">
      {railHeader}
      <div className="rail-header">
        <button className="rail-compute-btn" title="Compute settings" onClick={onOpenCompute}>
          Compute
          <span style={{ flex: 1 }} />
          <Cpu size={15} />
        </button>
      </div>
      <div className="rail-body">
        <button className="session-row new-session" onClick={() => setActiveId(null)}>
          <span className="session-title">New session</span>
          <Plus size={15} />
        </button>
        {sessions.map((s) => (
          <button
            key={s.id}
            className={`session-row ${s.id === activeId ? "active" : ""}`}
            title={`${HARNESS_LABELS[s.harness]}${s.model ? ` · ${s.model}` : ""}`}
            onClick={() => setActiveId(s.id)}
          >
            {state.busySessions.has(s.id) && <span className="busy-dot" />}
            <span className="session-title">{s.title?.trim() || "Untitled"}</span>
            <span className="session-time">{relTime(s.updatedAt)}</span>
            <span
              className="del"
              title="Delete session"
              onClick={(e) => {
                e.stopPropagation();
                void removeSession(s.id);
              }}
            >
              ×
            </span>
          </button>
        ))}
      </div>
    </aside>
  );

  return (
    <>
      {rail}
      <section className="chat-pane">
      {/* Header — browser-style tab strip of the open sessions. */}
      <div className="chat-header">
        <div className="tab-strip">
          {openTabs.map((id) => {
            const session = sessions.find((s) => s.id === id);
            return (
              <ClosableTab
                key={id}
                active={id === activeId}
                label={session?.title?.trim() || "Untitled"}
                icon={state.busySessions.has(id) ? <span className="busy-dot" /> : undefined}
                onSelect={() => setActiveId(id)}
                onClose={() => closeTab(id)}
              />
            );
          })}
          {/* Draft tab: the blank page has no session yet, so it can't be
              closed — selecting any other tab discards it. */}
          {activeId === null && (
            <button className="tab closable active" onClick={() => {}}>
              <span className="tab-label">New agent</span>
            </button>
          )}
          <button
            className="icon-btn"
            title="New agent"
            aria-label="New agent"
            onClick={() => setActiveId(null)}
          >
            <Plus size={14} />
          </button>
        </div>
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
          {messages.map((m) => (
            <Message key={m.id} message={m} onOpenFile={onOpenFile} />
          ))}
          {busy && (
            <div className="working">
              <span className="spinner" /> Working…
            </div>
          )}
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
              activeSession
                ? `Message ${HARNESS_LABELS[activeSession.harness]}…`
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
            <ModelPicker value={selection} onSelect={selectModel} onHarnesses={setHarnesses} />
            <div style={{ flex: 1 }} />
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
                <ArrowUp size={16} />
              </button>
            )}
          </div>
        </div>
      </div>
      </section>
    </>
  );
}
