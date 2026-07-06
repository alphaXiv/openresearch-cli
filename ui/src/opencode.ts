// Thin typed wrapper over the local opencode server, reached through orx up's
// /opencode/* reverse proxy. Shapes verified against opencode v1.17 /doc.

export interface TodoItem {
  content: string;
  status?: string;
  priority?: string;
}

export interface ToolState {
  status: "pending" | "running" | "completed" | "error";
  title?: string;
  output?: string;
  error?: string;
  input?: {
    todos?: TodoItem[];
    filePath?: string;
    content?: string;
    command?: string;
    description?: string;
    [k: string]: unknown;
  };
  metadata?: Record<string, unknown>;
}

export interface Part {
  id: string;
  messageID: string;
  sessionID?: string;
  type: string; // "text" | "reasoning" | "tool" | "step-start" | ...
  text?: string;
  tool?: string;
  state?: ToolState;
  [k: string]: unknown;
}

export interface MessageInfo {
  id: string;
  sessionID?: string;
  role: "user" | "assistant";
  time?: { created: number; completed?: number };
  tokens?: { input: number; output: number };
  cost?: number;
}

export interface MessageWithParts {
  info: MessageInfo;
  parts: Part[];
}

export interface Session {
  id: string;
  title?: string;
  time?: { created: number; updated: number };
  parentID?: string;
}

export interface SessionStatus {
  type: "idle" | "busy" | "retry";
}

export interface OpencodeEvent {
  type: string;
  properties: {
    part?: Part;
    info?: MessageInfo;
    sessionID?: string;
    status?: SessionStatus;
    messageID?: string;
    partID?: string;
    field?: string;
    delta?: string;
    [k: string]: unknown;
  };
}

const BASE = "/opencode";

async function json<T>(res: Response): Promise<T> {
  if (!res.ok) throw new Error(`opencode ${res.status}: ${await res.text().catch(() => "")}`);
  return (await res.json()) as T;
}

export const listSessions = () =>
  fetch(`${BASE}/session`)
    .then((r) => json<Session[]>(r))
    // Subagent sessions have a parentID; only top-level chats belong in the list.
    .then((sessions) => sessions.filter((s) => !s.parentID));

export const createSession = () =>
  fetch(`${BASE}/session`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: "{}",
  }).then((r) => json<Session>(r));

export const deleteSession = (sessionId: string) =>
  fetch(`${BASE}/session/${sessionId}`, { method: "DELETE" }).then((r) => r.ok);

export const getMessages = (sessionId: string) =>
  fetch(`${BASE}/session/${sessionId}/message`).then((r) => json<MessageWithParts[]>(r));

export const getSessionStatuses = () =>
  fetch(`${BASE}/session/status`)
    .then((r) => json<Record<string, SessionStatus>>(r))
    .catch(() => ({}) as Record<string, SessionStatus>);

/** Resolves when the turn finishes; streaming arrives via /event meanwhile. */
export const sendMessage = (sessionId: string, text: string) =>
  fetch(`${BASE}/session/${sessionId}/message`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ parts: [{ type: "text", text }] }),
  }).then((r) => json<MessageWithParts>(r));

export const abortSession = (sessionId: string) =>
  fetch(`${BASE}/session/${sessionId}/abort`, { method: "POST", body: "{}" }).then((r) => r.ok);

/** Global SSE stream; events are unnamed `message` events. Returns unsubscribe. */
export function subscribeEvents(onEvent: (event: OpencodeEvent) => void): () => void {
  const source = new EventSource(`${BASE}/event`);
  source.addEventListener("message", (e) => {
    try {
      onEvent(JSON.parse(e.data as string) as OpencodeEvent);
    } catch {
      // ignore keepalives
    }
  });
  return () => source.close();
}
