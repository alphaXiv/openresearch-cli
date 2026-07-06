// One EventSource('/api/events') for the whole app. Entity updates go to the
// caller's handlers; run.log deltas fan out through a tiny per-run emitter so
// terminals can subscribe without threading props everywhere.

import { useEffect, useRef } from "react";
import type { Experiment, Project, Run } from "./api";

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

export interface OrxEventHandlers {
  onRun: (run: Run) => void;
  onExperiment: (experiment: Experiment) => void;
  onProject: (project: Project) => void;
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
    es.addEventListener("run.log", (e) => {
      const d = parse<RunLogEvent>(e as MessageEvent);
      if (d?.runId) emitRunLog(d);
    });
    return () => es.close();
  }, []);
}
