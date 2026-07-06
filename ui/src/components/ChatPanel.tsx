import { ArrowUp, Plus, X } from "lucide-react";
import { useCallback, useEffect, useReducer, useRef, useState } from "react";
import * as oc from "../opencode";
import { ClosableTab } from "./ClosableTab";
import { Md } from "./Md";

// --- chat state --------------------------------------------------------------

interface ChatState {
  messagesBySession: Record<string, oc.MessageWithParts[]>;
  busySessions: Set<string>;
}

type Action =
  | { type: "reset" }
  | { type: "seed"; sessionId: string; messages: oc.MessageWithParts[] }
  | { type: "messageUpdated"; sessionId: string; info: oc.MessageInfo }
  | { type: "partUpdated"; sessionId: string; part: oc.Part }
  | { type: "partDelta"; sessionId: string; messageId: string; partId: string; field: string; delta: string }
  | { type: "optimisticUser"; sessionId: string; text: string }
  | { type: "busy"; sessionId: string; busy: boolean }
  | { type: "seedBusy"; sessions: string[] };

const LOCAL_PREFIX = "local-";

function upsertMessage(list: oc.MessageWithParts[], info: oc.MessageInfo): oc.MessageWithParts[] {
  const i = list.findIndex((m) => m.info.id === info.id);
  if (i >= 0) {
    const next = list.slice();
    next[i] = { ...next[i], info };
    return next;
  }
  // A real user message replaces any optimistic local one.
  const cleaned =
    info.role === "user" ? list.filter((m) => !m.info.id.startsWith(LOCAL_PREFIX)) : list;
  return [...cleaned, { info, parts: [] }];
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
    case "messageUpdated": {
      const list = state.messagesBySession[action.sessionId] ?? [];
      return {
        ...state,
        messagesBySession: {
          ...state.messagesBySession,
          [action.sessionId]: upsertMessage(list, action.info),
        },
      };
    }
    case "partUpdated": {
      const list = state.messagesBySession[action.sessionId] ?? [];
      const mi = list.findIndex((m) => m.info.id === action.part.messageID);
      if (mi < 0) return state;
      const msg = list[mi];
      const pi = msg.parts.findIndex((p) => p.id === action.part.id);
      const parts =
        pi >= 0
          ? [...msg.parts.slice(0, pi), action.part, ...msg.parts.slice(pi + 1)]
          : [...msg.parts, action.part];
      const next = list.slice();
      next[mi] = { ...msg, parts };
      return {
        ...state,
        messagesBySession: { ...state.messagesBySession, [action.sessionId]: next },
      };
    }
    case "partDelta": {
      const list = state.messagesBySession[action.sessionId] ?? [];
      const mi = list.findIndex((m) => m.info.id === action.messageId);
      if (mi < 0) return state;
      const msg = list[mi];
      const pi = msg.parts.findIndex((p) => p.id === action.partId);
      if (pi < 0) return state;
      const part = msg.parts[pi];
      const prev = part[action.field];
      const updated: oc.Part = {
        ...part,
        [action.field]: (typeof prev === "string" ? prev : "") + action.delta,
      };
      const parts = [...msg.parts.slice(0, pi), updated, ...msg.parts.slice(pi + 1)];
      const next = list.slice();
      next[mi] = { ...msg, parts };
      return {
        ...state,
        messagesBySession: { ...state.messagesBySession, [action.sessionId]: next },
      };
    }
    case "optimisticUser": {
      const list = state.messagesBySession[action.sessionId] ?? [];
      const msg: oc.MessageWithParts = {
        info: { id: `${LOCAL_PREFIX}${Date.now()}`, role: "user" },
        parts: [
          {
            id: `${LOCAL_PREFIX}part`,
            messageID: `${LOCAL_PREFIX}${Date.now()}`,
            type: "text",
            text: action.text,
          },
        ],
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

function toolStatusClass(status: string | undefined): string {
  if (status === "error") return "tool-status error";
  if (status === "completed") return "tool-status";
  return "tool-status running"; // pending/running = in-flight
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

function toolSummary(part: oc.Part): string {
  const input = part.state?.input;
  if (input?.command) return input.command;
  if (input?.filePath) return input.filePath;
  if (input?.description) return input.description;
  return part.state?.title ?? "";
}

function ToolRow({ part }: { part: oc.Part }) {
  const state = part.state;
  const output = state?.error || state?.output || "";
  // todowrite gets its checklist rendered instead of raw output.
  const todos = part.tool === "todowrite" ? (state?.input?.todos ?? []) : null;
  return (
    <details className="tool-row">
      <summary>
        <span className={toolStatusClass(state?.status)} />
        <span className="tool-name">{part.tool ?? "tool"}</span>
        <span className="tool-cmd">{toolSummary(part)}</span>
      </summary>
      {todos && todos.length > 0 ? (
        <div className="tool-output">
          {todos.map((t, i) => (
            <div key={i}>
              {t.status === "completed" ? "[x]" : t.status === "in_progress" ? "[~]" : "[ ]"}{" "}
              {t.content}
            </div>
          ))}
        </div>
      ) : output ? (
        <div className="tool-output">{output.slice(0, 20000)}</div>
      ) : null}
    </details>
  );
}

function Message({ message }: { message: oc.MessageWithParts }) {
  if (message.info.role === "user") {
    const text = message.parts
      .filter((p) => p.type === "text")
      .map((p) => p.text ?? "")
      .join("\n");
    return (
      <div className="msg-user">
        <span className="eyebrow">You</span>
        {text}
      </div>
    );
  }
  return (
    <div className="msg-assistant">
      {message.parts.map((part) => {
        if (part.type === "text" && part.text) return <Md key={part.id} text={part.text} />;
        if (part.type === "reasoning" && part.text)
          return (
            <details key={part.id} className="reasoning">
              <summary>thinking…</summary>
              {part.text}
            </details>
          );
        if (part.type === "tool") return <ToolRow key={part.id} part={part} />;
        return null;
      })}
    </div>
  );
}

// --- panel -------------------------------------------------------------------

export function ChatPanel({
  projectId,
  agentRunning,
  onRetryAgent,
}: {
  projectId: string;
  agentRunning: boolean;
  onRetryAgent: () => void;
}) {
  const [sessions, setSessions] = useState<oc.Session[]>([]);
  const [activeId, setActiveId] = useState<string | null>(null);
  // Sessions open as tabs in the chat header, in strip order. Selecting a
  // session (rail or strip) opens a tab; closing one only removes it here.
  const [openTabs, setOpenTabs] = useState<string[]>([]);
  const [draft, setDraft] = useState("");
  const [state, dispatch] = useReducer(reducer, {
    messagesBySession: {},
    busySessions: new Set<string>(),
  });
  const loadedSessions = useRef(new Set<string>());
  const threadRef = useRef<HTMLDivElement>(null);
  const stickToBottom = useRef(true);

  // Reset everything when the project (and thus the opencode process) changes.
  useEffect(() => {
    setSessions([]);
    setActiveId(null);
    setOpenTabs([]);
    setDraft("");
    dispatch({ type: "reset" });
    loadedSessions.current = new Set();
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

  const refreshSessions = useCallback(async () => {
    try {
      const list = await oc.listSessions();
      list.sort((a, b) => (b.time?.updated ?? 0) - (a.time?.updated ?? 0));
      setSessions(list);
      setActiveId((cur) => cur ?? list[0]?.id ?? null);
      const statuses = await oc.getSessionStatuses();
      dispatch({
        type: "seedBusy",
        sessions: Object.entries(statuses)
          .filter(([, s]) => s.type === "busy" || s.type === "retry")
          .map(([id]) => id),
      });
    } catch {
      // agent not up yet; caller re-renders when it is
    }
  }, []);

  useEffect(() => {
    if (agentRunning) void refreshSessions();
  }, [agentRunning, projectId, refreshSessions]);

  // Load message history when a session becomes active.
  useEffect(() => {
    if (!activeId || loadedSessions.current.has(activeId)) return;
    loadedSessions.current.add(activeId);
    oc.getMessages(activeId)
      .then((messages) => dispatch({ type: "seed", sessionId: activeId, messages }))
      .catch(() => loadedSessions.current.delete(activeId));
  }, [activeId]);

  // Global opencode event stream → reducer.
  useEffect(() => {
    if (!agentRunning) return;
    const unsubscribe = oc.subscribeEvents((event) => {
      const p = event.properties;
      switch (event.type) {
        case "message.updated":
          if (p.info?.sessionID)
            dispatch({ type: "messageUpdated", sessionId: p.info.sessionID, info: p.info });
          break;
        case "message.part.updated": {
          const sid = p.part?.sessionID ?? p.sessionID;
          if (p.part && sid) dispatch({ type: "partUpdated", sessionId: sid, part: p.part });
          break;
        }
        case "message.part.delta":
          if (p.sessionID && p.messageID && p.partID && p.field)
            dispatch({
              type: "partDelta",
              sessionId: p.sessionID,
              messageId: p.messageID,
              partId: p.partID,
              field: p.field,
              delta: p.delta ?? "",
            });
          break;
        case "session.status":
          if (p.sessionID)
            dispatch({
              type: "busy",
              sessionId: p.sessionID,
              busy: p.status?.type === "busy" || p.status?.type === "retry",
            });
          break;
        case "session.idle":
          if (p.sessionID) dispatch({ type: "busy", sessionId: p.sessionID, busy: false });
          break;
        case "session.updated":
          if (p.info) {
            const info = p.info as unknown as oc.Session;
            setSessions((cur) => {
              const i = cur.findIndex((s) => s.id === info.id);
              if (i < 0) return info.parentID ? cur : [info, ...cur];
              const next = cur.slice();
              next[i] = info;
              return next;
            });
          }
          break;
      }
    });
    return unsubscribe;
  }, [agentRunning, projectId]);

  const messages = activeId ? (state.messagesBySession[activeId] ?? []) : [];
  const busy = activeId ? state.busySessions.has(activeId) : false;

  // Autoscroll while pinned to the bottom.
  useEffect(() => {
    const el = threadRef.current;
    if (el && stickToBottom.current) el.scrollTop = el.scrollHeight;
  }, [messages, busy]);

  async function send() {
    const text = draft.trim();
    if (!text || !agentRunning) return;
    setDraft("");
    let sid = activeId;
    try {
      if (!sid) {
        const session = await oc.createSession();
        loadedSessions.current.add(session.id);
        setSessions((cur) => [session, ...cur]);
        setActiveId(session.id);
        sid = session.id;
      }
      dispatch({ type: "optimisticUser", sessionId: sid, text });
      dispatch({ type: "busy", sessionId: sid, busy: true });
      stickToBottom.current = true;
      await oc.sendMessage(sid, text);
    } catch {
      if (sid) dispatch({ type: "busy", sessionId: sid, busy: false });
    } finally {
      if (sid) dispatch({ type: "busy", sessionId: sid, busy: false });
    }
  }

  function stop() {
    if (activeId) void oc.abortSession(activeId);
  }

  async function removeSession(id: string) {
    await oc.deleteSession(id).catch(() => false);
    setSessions((cur) => cur.filter((s) => s.id !== id));
    closeTab(id);
  }

  const rail = (
    <aside className="session-rail">
      <div className="rail-header">
        <div className="rail-title">Agents</div>
        <button
          className="icon-btn"
          title="New session"
          aria-label="New session"
          onClick={() => setActiveId(null)}
        >
          <Plus size={14} />
        </button>
      </div>
      <div className="rail-body">
        {sessions.length === 0 && (
          <div style={{ padding: "10px 12px", fontSize: 12, color: "var(--muted)" }}>
            No sessions yet
          </div>
        )}
        {sessions.map((s) => (
          <button
            key={s.id}
            className={`session-row ${s.id === activeId ? "active" : ""}`}
            onClick={() => setActiveId(s.id)}
          >
            {state.busySessions.has(s.id) && <span className="busy-dot" />}
            <span className="session-title">{s.title?.trim() || "Untitled"}</span>
            <span className="session-time">{relTime(s.time?.updated)}</span>
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

  if (!agentRunning) {
    return (
      <>
        {rail}
        <section className="chat-pane">
          <div className="chat-empty">
            <h2>
              Open<span>Research</span>
            </h2>
            <p>The research agent is not running.</p>
            <button className="btn" onClick={onRetryAgent}>
              Start agent
            </button>
          </div>
        </section>
      </>
    );
  }

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
            <Message key={m.info.id} message={m} />
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
          <textarea
            value={draft}
            placeholder="Ask the research agent…"
            rows={2}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                void send();
              }
            }}
          />
          <div className="composer-actions">
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
                disabled={!draft.trim()}
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
