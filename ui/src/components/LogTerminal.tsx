import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import { useEffect, useRef } from "react";
import { fetchLog } from "../api";
import { onRunLog } from "../events";

function b64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

/**
 * Live log terminal for one run. Backfills from /api/runs/{id}/log, then
 * follows `run.log` SSE deltas. Fast path writes an in-order delta directly;
 * any gap (missed event, reconnect) falls back to a serialized fetch-sync
 * from the current byte offset, so output is never duplicated or reordered.
 */
export function LogTerminal({ runId }: { runId: string }) {
  const wrapRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const wrap = wrapRef.current;
    if (!wrap) return;

    const term = new Terminal({
      convertEol: true,
      disableStdin: true,
      fontSize: 12,
      fontFamily: 'ui-monospace, "SF Mono", Menlo, Consolas, monospace',
      scrollback: 20000,
      theme: {
        background: "#1a1a1a",
        foreground: "#e6e1e0",
        cursor: "#1a1a1a",
        selectionBackground: "#2c3441",
      },
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(wrap);
    try {
      fit.fit();
    } catch {
      // container may have zero size briefly
    }
    const observer = new ResizeObserver(() => {
      try {
        fit.fit();
      } catch {
        // ignore
      }
    });
    observer.observe(wrap);

    let disposed = false;
    let nextOffset = 0;
    let syncing = false;
    let syncAgain = false;

    async function sync() {
      if (syncing) {
        syncAgain = true;
        return;
      }
      syncing = true;
      try {
        for (;;) {
          const chunk = await fetchLog(runId, nextOffset);
          if (disposed) return;
          if (chunk.dataBase64) term.write(b64ToBytes(chunk.dataBase64));
          nextOffset = chunk.nextOffset;
          if (chunk.eof) break;
        }
      } catch {
        // transient; the next run.log event retries
      } finally {
        syncing = false;
        if (syncAgain && !disposed) {
          syncAgain = false;
          void sync();
        }
      }
    }

    const unsubscribe = onRunLog(runId, (ev) => {
      if (disposed) return;
      const bytes = b64ToBytes(ev.dataBase64);
      if (!syncing && ev.offset === nextOffset) {
        term.write(bytes);
        nextOffset += bytes.length;
      } else if (ev.offset + bytes.length > nextOffset) {
        void sync();
      }
    });
    void sync();

    return () => {
      disposed = true;
      unsubscribe();
      observer.disconnect();
      term.dispose();
    };
  }, [runId]);

  return <div ref={wrapRef} style={{ width: "100%", height: "100%" }} />;
}
