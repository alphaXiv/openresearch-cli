import { useEffect, useState } from "react";
import { getChatMessages, type ChatMessage, type ChatPart } from "../api";
import { onChatEvent } from "../events";
import { findPartById, SubagentTranscript } from "./ChatPanel";

/** Right-pane tab body for a sub-agent transcript. The spawn part (and its
 * streamed `children`) lives on the parent session's chat messages, so this
 * seeds from `getChatMessages` and then follows the live `chat.message` stream —
 * the same source the inline block renders from, so it stays in sync as the
 * sub-agent works. No dedicated fetch endpoint needed. */
export function SubagentTab({
  sessionId,
  spawnPartId,
  onOpenFile,
  onOpenSubagent,
}: {
  sessionId: string;
  spawnPartId: string;
  onOpenFile?: (path: string) => void;
  onOpenSubagent?: (spawnPartId: string) => void;
}) {
  const [messages, setMessages] = useState<ChatMessage[] | null>(null);

  useEffect(() => {
    let live = true;
    getChatMessages(sessionId)
      .then((m) => live && setMessages(m))
      .catch(() => live && setMessages([]));
    // Live updates: replace the message the event carries (assistant turns
    // re-broadcast the whole message on every flush).
    const off = onChatEvent((ev) => {
      if (ev.type !== "message" || ev.sessionId !== sessionId) return;
      setMessages((prev) => {
        const next = prev ? prev.slice() : [];
        const idx = next.findIndex((m) => m.id === ev.message.id);
        if (idx === -1) next.push(ev.message);
        else next[idx] = ev.message;
        return next;
      });
    });
    return () => {
      live = false;
      off();
    };
  }, [sessionId]);

  if (messages === null) {
    return (
      <div className="tab-body">
        <div className="pane-content subagent-tab-content">
          <div className="subagent-empty">Loading…</div>
        </div>
      </div>
    );
  }

  // Locate the spawn part across all messages; its `children` are the transcript.
  let spawn: ChatPart | null = null;
  for (const m of messages) {
    spawn = findPartById(m.parts, spawnPartId);
    if (spawn) break;
  }

  return (
    <div className="tab-body">
      <div className="pane-content subagent-tab-content">
        {spawn ? (
          <SubagentTranscript
            spawn={spawn}
            onOpenFile={onOpenFile}
            onOpenSubagent={onOpenSubagent}
          />
        ) : (
          <div className="subagent-empty">This sub-agent is no longer available.</div>
        )}
      </div>
    </div>
  );
}
