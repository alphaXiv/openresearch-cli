// The live view of a chat session's private worktree — what the agent is
// changing right now, before any run/commit exists (the Code tab is
// committed-state only). Two segmented views, both bound to the session the tab
// was opened from:
//
//   Changes (default): the unified diff vs the baseline merge-base, untracked
//     files included as new-file chunks — the same per-file-card rendering as
//     the experiment Changes view (the header's file count comes from a
//     separate git pass, so it stays truthful even when the diff truncates).
//   Files: the full live worktree tree (CodeTab's shared components), clicks
//     opening the existing FileViewer against this session's worktree.
//
// Freshness without idle churn: poll every 5 s only while the session is busy
// (chat.busy SSE), refresh once on the busy→idle edge, and a manual refresh
// button always works. Transient errors (an index.lock race while the agent
// commits) keep the last-good data with a small "refresh failed" note, mirroring
// CodeTab's staleness handling.

import { FolderGit2, GitBranch, RotateCw } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  getCodeTree,
  getSessionWorktree,
  listChatSessions,
  type CodeTree,
  type SessionWorktree,
} from "../api";
import { onChatEvent } from "../events";
import { buildTree, TreeLevel } from "./codeTree";
import { GitDiff, TruncatedDiffNotice } from "./GitDiff";

/** Poll cadence while the session's agent is working (matches the working-tree
 * poll in DetailDrawer). */
const POLL_MS = 5000;

export type WorktreeView = "changes" | "files";

export function WorktreeTab({
  sessionId,
  projectId,
  view,
  toggled,
  onViewChange,
  onToggledChange,
  onOpenFile,
}: {
  sessionId: string;
  projectId: string;
  /** Which segmented view is showing (lives on the tab def, so it survives the
   * unmount/remount when another right-pane tab fronts this one). */
  view: WorktreeView;
  /** Files-view dirs flipped away from their depth default (on the tab def). */
  toggled: ReadonlySet<string>;
  onViewChange: (view: WorktreeView) => void;
  onToggledChange: (toggled: ReadonlySet<string>) => void;
  /** Open a file in the right pane's FileViewer, keyed to this worktree. */
  onOpenFile: (path: string, sessionId: string) => void;
}) {
  const [wt, setWt] = useState<SessionWorktree | null>(null);
  const [tree, setTree] = useState<CodeTree | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  // A request id drops stale responses — superseded refreshes, poll ticks, and
  // (via the effect-cleanup bump) post-unmount completions.
  const reqId = useRef(0);

  const load = useCallback(() => {
    const id = ++reqId.current;
    setLoading(true);
    // Both fetches run every load: the Changes list and the Files tree share
    // one refresh so switching views never shows a stale half.
    Promise.all([getSessionWorktree(sessionId), getCodeTree(projectId, { sessionId })])
      .then(([w, t]) => {
        if (id !== reqId.current) return;
        setWt(w);
        setTree(t);
        setError(null);
      })
      .catch((e: Error) => {
        if (id !== reqId.current) return;
        // Keep the last-good data — a transient git failure (index.lock while
        // the agent commits) shouldn't blank the view.
        setError(e.message);
      })
      .finally(() => {
        if (id === reqId.current) setLoading(false);
      });
  }, [sessionId, projectId]);

  // Fetch on mount and whenever the bound session changes; the cleanup bump
  // invalidates in-flight responses on session change and unmount.
  useEffect(() => {
    setWt(null);
    setTree(null);
    setError(null);
    load();
    return () => {
      reqId.current++;
    };
  }, [load]);

  // Poll only while this session is busy, and refresh once on the busy→idle
  // edge (the final state after a turn). No idle polling — committed/quiescent
  // worktrees don't move, which is what made the original always-on session
  // mode wasteful.
  useEffect(() => {
    let busy = false;
    // Once any edge arrives for this session it supersedes the mount-time
    // snapshot below (which may resolve later, out of date).
    let edgeSeen = false;
    let disposed = false;
    let timer: ReturnType<typeof setInterval> | null = null;
    const start = () => {
      if (timer) return;
      timer = setInterval(load, POLL_MS);
    };
    const stop = () => {
      if (timer) {
        clearInterval(timer);
        timer = null;
      }
    };
    const off = onChatEvent((ev) => {
      if (ev.type !== "busy" || ev.sessionId !== sessionId) return;
      edgeSeen = true;
      if (ev.busy && !busy) {
        busy = true;
        start();
      } else if (!ev.busy && busy) {
        busy = false;
        stop();
        load(); // catch the final post-turn state
      }
    });
    // chat.busy is edge-only: a tab opened mid-turn would never see a
    // busy:true edge, so polling (and the gated busy→idle refresh) would sit
    // out the whole turn. Seed from the session list's busy snapshot instead.
    listChatSessions(projectId)
      .then((sessions) => {
        if (disposed || edgeSeen || busy) return;
        if (sessions.find((s) => s.id === sessionId)?.busy) {
          busy = true;
          start();
        }
      })
      .catch(() => {});
    return () => {
      disposed = true;
      off();
      stop();
    };
    // load is memoized on [sessionId, projectId], which the closure also reads.
  }, [sessionId, projectId, load]);

  const filesTree = useMemo(() => (tree ? buildTree(tree.entries) : null), [tree]);

  const toggle = useCallback(
    (path: string) => {
      const next = new Set(toggled);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      onToggledChange(next);
    },
    [toggled, onToggledChange],
  );

  const branchChip =
    wt?.branch ?? (wt?.baselineBranch ? `detached @ ${wt.baselineBranch}` : "detached");
  const fileCount = wt?.files?.length ?? 0;

  return (
    <div className="code-tab wt-tab">
      <div className="code-tab-header">
        <div className="seg">
          <button className={view === "changes" ? "active" : ""} onClick={() => onViewChange("changes")}>
            Changes
          </button>
          <button className={view === "files" ? "active" : ""} onClick={() => onViewChange("files")}>
            Files
          </button>
        </div>
        {wt?.exists && (
          <span className="wt-branch-chip" title={branchChip}>
            <GitBranch size={12} />
            <span className="wt-branch-name">{branchChip}</span>
          </span>
        )}
        {wt?.exists && view === "changes" && (
          <span className="code-tab-note wt-count">
            {fileCount} {fileCount === 1 ? "file" : "files"}
          </span>
        )}
        <span style={{ flex: 1 }} />
        <button className="icon-btn" title="Refresh" aria-label="Refresh" onClick={load}>
          {loading ? <span className="spinner" /> : <RotateCw size={13} />}
        </button>
      </div>
      {error && (wt || tree) && <div className="code-tab-note">Refresh failed: {error}</div>}
      {!wt ? (
        <div className="code-tab-body">
          <div className="code-tab-note">{error ? `Failed to load: ${error}` : "Loading…"}</div>
        </div>
      ) : !wt.exists ? (
        <div className="code-tab-body">
          <div className="wt-empty">
            <FolderGit2 size={22} />
            <p>The agent hasn't started working yet — its worktree is created on the first message.</p>
          </div>
        </div>
      ) : view === "changes" ? (
        <div className="code-tab-body wt-changes">
          {fileCount === 0 || !wt.diff ? (
            <div className="changes-note">No changes yet.</div>
          ) : (
            <>
              {wt.diff.truncated && (
                <TruncatedDiffNotice bytesRead={wt.diff.bytesRead} byteLimit={wt.diff.byteLimit} />
              )}
              <GitDiff diff={wt.diff.diff} />
            </>
          )}
        </div>
      ) : (
        <div className="code-tab-body">
          {tree?.root === "clone" && (
            <div className="code-tab-note">Worktree unavailable — showing the project clone.</div>
          )}
          {tree?.truncated && <div className="code-tab-note">listing truncated</div>}
          {!filesTree ? (
            <div className="code-tab-note">Loading…</div>
          ) : filesTree.dirs.size === 0 && filesTree.files.length === 0 ? (
            <div className="code-tab-note">No files.</div>
          ) : (
            <div className="code-tree">
              <TreeLevel
                node={filesTree}
                parentPath=""
                depth={0}
                toggled={toggled}
                onToggle={toggle}
                onOpenFile={(path) => onOpenFile(path, sessionId)}
              />
            </div>
          )}
        </div>
      )}
    </div>
  );
}
