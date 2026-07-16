// One EventSource('/api/events') for the whole app. Entity updates go to the
// caller's handlers; run.log deltas fan out through a tiny per-run emitter so
// terminals can subscribe without threading props everywhere.

import { useEffect, useRef } from "react";
import type { ChatMessage, ChatSession, Experiment, Project, Run } from "./api";

export interface RunLogEvent {
  runId: string;
  dataBase64: string;
  offset: number;
}

type LogListener = (ev: RunLogEvent) => void;
const logListeners = new Map<string, Set<LogListener>>();

export function onRunLog(runId: string, fn: LogListener): () => void {
  let set = logListeners.get(runId);
  if (!set) {
    set = new Set();
    logListeners.set(runId, set);
  }
  set.add(fn);
  return () => {
    set.delete(fn);
    if (set.size === 0) logListeners.delete(runId);
  };
}

function emitRunLog(ev: RunLogEvent) {
  logListeners.get(ev.runId)?.forEach((fn) => fn(ev));
}

// Chat events fan out the same way so ChatPanel shares the one EventSource.
export type ChatEvent =
  | { type: "session"; session: ChatSession }
  | { type: "sessionDeleted"; sessionId: string }
  | { type: "message"; sessionId: string; message: ChatMessage }
  | { type: "busy"; sessionId: string; busy: boolean };

type ChatListener = (ev: ChatEvent) => void;
const chatListeners = new Set<ChatListener>();

export function onChatEvent(fn: ChatListener): () => void {
  chatListeners.add(fn);
  return () => {
    chatListeners.delete(fn);
  };
}

function emitChat(ev: ChatEvent) {
  chatListeners.forEach((fn) => fn(ev));
}

// Data-dir move progress fans out the same way so the Storage settings card can
// subscribe without touching the shared useOrxEvents handler set.
export type DataDirMoveEvent =
  | { type: "progress"; phase: string; copiedBytes: number; totalBytes: number }
  | { type: "done"; path: string; oldPathLeft?: string }
  | { type: "error"; error: string };

type DataDirMoveListener = (ev: DataDirMoveEvent) => void;
const dataDirMoveListeners = new Set<DataDirMoveListener>();

export function onDataDirMove(fn: DataDirMoveListener): () => void {
  dataDirMoveListeners.add(fn);
  return () => {
    dataDirMoveListeners.delete(fn);
  };
}

function emitDataDirMove(ev: DataDirMoveEvent) {
  dataDirMoveListeners.forEach((fn) => fn(ev));
}

export interface OrxEventHandlers {
  onRun: (run: Run) => void;
  onExperiment: (experiment: Experiment) => void;
  onProject: (project: Project) => void;
  /** The project's files dir changed on disk — refetch the listing. */
  onFiles?: (projectId: string) => void;
}

export function useOrxEvents(handlers: OrxEventHandlers) {
  // Keep the latest handlers without re-opening the stream every render.
  const ref = useRef(handlers);
  ref.current = handlers;
  useEffect(() => {
    const es = new EventSource("/api/events");
    const parse = <T>(e: MessageEvent): T | null => {
      try {
        return JSON.parse(e.data as string) as T;
      } catch {
        return null;
      }
    };
    es.addEventListener("run.updated", (e) => {
      const d = parse<{ run: Run }>(e as MessageEvent);
      if (d?.run) ref.current.onRun(d.run);
    });
    es.addEventListener("experiment.updated", (e) => {
      const d = parse<{ experiment: Experiment }>(e as MessageEvent);
      if (d?.experiment) ref.current.onExperiment(d.experiment);
    });
    es.addEventListener("project.updated", (e) => {
      const d = parse<{ project: Project }>(e as MessageEvent);
      if (d?.project) ref.current.onProject(d.project);
    });
    es.addEventListener("files.updated", (e) => {
      const d = parse<{ projectId: string }>(e as MessageEvent);
      if (d?.projectId) ref.current.onFiles?.(d.projectId);
    });
    es.addEventListener("run.log", (e) => {
      const d = parse<RunLogEvent>(e as MessageEvent);
      if (d?.runId) emitRunLog(d);
    });
    es.addEventListener("chat.session", (e) => {
      const d = parse<{ session: ChatSession }>(e as MessageEvent);
      if (d?.session) emitChat({ type: "session", session: d.session });
    });
    es.addEventListener("chat.session.deleted", (e) => {
      const d = parse<{ sessionId: string }>(e as MessageEvent);
      if (d?.sessionId) emitChat({ type: "sessionDeleted", sessionId: d.sessionId });
    });
    es.addEventListener("chat.message", (e) => {
      const d = parse<{ sessionId: string; message: ChatMessage }>(e as MessageEvent);
      if (d?.message) emitChat({ type: "message", sessionId: d.sessionId, message: d.message });
    });
    es.addEventListener("chat.busy", (e) => {
      const d = parse<{ sessionId: string; busy: boolean }>(e as MessageEvent);
      if (d?.sessionId) emitChat({ type: "busy", sessionId: d.sessionId, busy: d.busy });
    });
    es.addEventListener("datadir.move.progress", (e) => {
      const d = parse<{ phase: string; copiedBytes: number; totalBytes: number }>(
        e as MessageEvent,
      );
      if (d) emitDataDirMove({ type: "progress", ...d });
    });
    es.addEventListener("datadir.move.done", (e) => {
      const d = parse<{ path: string; oldPathLeft?: string }>(e as MessageEvent);
      if (d) emitDataDirMove({ type: "done", path: d.path, oldPathLeft: d.oldPathLeft });
    });
    es.addEventListener("datadir.move.error", (e) => {
      const d = parse<{ error: string }>(e as MessageEvent);
      if (d) emitDataDirMove({ type: "error", error: d.error });
    });
    return () => es.close();
  }, []);
}
