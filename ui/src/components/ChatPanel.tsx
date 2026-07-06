import { useCallback, useEffect, useReducer, useRef, useState } from "react";
import * as oc from "../opencode";
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

function toolStatusColor(status: string | undefined): string {
  switch (status) {
    case "completed":
      return "var(--green)";
    case "error":
      return "var(--red)";
    case "running":
      return "var(--teal)";
    default:
      return "var(--amber)";
  }
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
        <span className="tool-status" style={{ background: toolStatusColor(state?.status) }} />
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
    return <div className="msg-user">{text}</div>;
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
  const [listOpen, setListOpen] = useState(false);
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
    setDraft("");
    dispatch({ type: "reset" });
    loadedSessions.current = new Set();
  }, [projectId]);

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
    setActiveId((cur) => (cur === id ? null : cur));
  }

  const activeSession = sessions.find((s) => s.id === activeId);

  if (!agentRunning) {
    return (
      <div className="chat-pane">
        <div className="chat-empty">
          <h2>
            Open<span>Research</span>
          </h2>
          <p>The research agent is not running.</p>
          <button className="btn" onClick={onRetryAgent}>
            Start agent
          </button>
        </div>
      </div>
    );
  }

  return (
    <div className="chat-pane">
      <div className="chat-header">
        <button
          className="btn ghost sm"
          onClick={() => setListOpen((v) => !v)}
          title="Sessions"
        >
          {listOpen ? "▾" : "▸"} Sessions ({sessions.length})
        </button>
        <div className="title">{activeSession?.title?.trim() || (activeId ? "Untitled" : "New agent")}</div>
        <button
          className="btn ghost sm"
          title="New session"
          onClick={() => {
            setActiveId(null);
            setListOpen(false);
          }}
        >
          +
        </button>
      </div>

      {listOpen && (
        <div className="session-list">
          {sessions.length === 0 && (
            <div style={{ padding: "10px 12px", fontSize: 12, color: "var(--muted)" }}>
              No sessions yet
            </div>
          )}
          {sessions.map((s) => (
            <button
              key={s.id}
              className={`session-row ${s.id === activeId ? "active" : ""}`}
              onClick={() => {
                setActiveId(s.id);
                setListOpen(false);
              }}
            >
              {state.busySessions.has(s.id) && <span className="busy-dot" />}
              <span className="session-title">{s.title?.trim() || "Untitled"}</span>
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
      )}

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
            {busy && (
              <button className="btn sm danger" onClick={stop}>
                Stop
              </button>
            )}
            <button className="btn sm primary" onClick={() => void send()} disabled={!draft.trim()}>
              Send
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
