import {
  FileCode,
  FolderGit2,
  FolderTree,
  GitBranch,
  Maximize2,
  Minimize2,
  ScrollText,
  Terminal,
  X,
} from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import {
  cancelRun,
  getFiles,
  listExperiments,
  listProjects,
  listRuns,
  openProject,
  type Experiment,
  type ProjectFiles,
  type Project,
  type Run,
} from "./api";
import { ChatPanel } from "./components/ChatPanel";
import { CodeTab } from "./components/CodeTab";
import { WorktreeTab, type WorktreeView } from "./components/WorktreeTab";
import { FilesTab } from "./components/FilesTab";
import { ClosableTab } from "./components/ClosableTab";
import { DetailDrawer, type ExperimentView } from "./components/DetailDrawer";
import { FileViewer } from "./components/FileViewer";
import { RailHeader } from "./components/Header";
import { Onboarding } from "./components/Onboarding";
import { ProjectsHome } from "./components/ProjectsHome";
import { RunsTable } from "./components/RunsTable";
import { Md } from "./components/Md";
import { SettingsView, type SettingsTab } from "./components/SettingsPage";
import { Tour, TOUR_DONE_KEY } from "./components/Tour";
import { TreeView } from "./components/TreeView";
import { onChatEvent, useOrxEvents } from "./events";

/** An experiment view open as a right-panel tab. */
interface ExpViewDef {
  id: string;
  view: ExperimentView;
}

const sameExpTab = (a: ExpViewDef, b: ExpViewDef) => a.id === b.id && a.view === b.view;

/** A project file open as a right-panel tab (clicked in chat tool rows or the
 * code browser). */
interface FileViewDef {
  path: string;
  /** Which backend serves this file. Absent/"repo" → the repo `/file`
   * endpoint (worktree/clone/branch); "files" → the project's files dir
   * (`/files/report` + `/files/file`). */
  source?: "repo" | "files";
  /** Chat session whose worktree holds the file (absent → hub clone).
   * Files-dir tabs never carry this. */
  sessionId?: string;
  /** Branch whose committed copy to show (code browser in branch mode);
   * overrides the live checkout. */
  ref?: string;
}

const sameFileTab = (a: FileViewDef, b: FileViewDef) =>
  a.path === b.path &&
  (a.source ?? "repo") === (b.source ?? "repo") &&
  a.sessionId === b.sessionId &&
  a.ref === b.ref;

const fileTabKey = (t: FileViewDef) =>
  `${t.source ?? "repo"}:${t.sessionId ?? ""}:${t.ref ?? ""}:${t.path}`;

/** A proposed plan open as a right-panel tab (from the chat plan strip/card).
 * The markdown is already client-side (it rode the prompt part), so the tab
 * renders it directly — no fetch. Deliberately has neither a `view` nor a
 * `path` field: the other tab kinds discriminate on those. */
interface PlanViewDef {
  kind: "plan";
  sessionId: string;
  /** The prompt part the plan came from — one tab per plan card. */
  promptId: string;
  plan: string;
}

/** The project's code-browser tab (at most one): an experiment branch's
 * committed tree, or the hub clone's checkout, opened from an experiment
 * card's Code shortcut. Source + expansion state live here — CodeTab
 * unmounts whenever another right-pane tab fronts it (e.g. clicking a
 * file), and remount must not lose them. Discriminates on the `code` flag
 * (the other tab kinds discriminate on `id`/`path`/`kind`/`wt`). */
interface CodeTabDef {
  code: true;
  /** Source to browse: "" = the project clone, else a branch name. */
  sel: string;
  /** Dirs the user flipped away from their depth default. */
  toggled: ReadonlySet<string>;
}

/** The live session-worktree tab (at most one): what a chat session's agent is
 * changing right now, opened from the chat header. Bound to one session — the
 * Changes/Files view and Files-tree expansion state live here so the tab
 * survives WorktreeTab's unmount/remount when another right-pane tab fronts it.
 * Discriminates on the `wt` flag. */
interface WorktreeTabDef {
  wt: true;
  /** The chat session this tab watches. */
  sessionId: string;
  /** Which segmented view is showing. */
  view: WorktreeView;
  /** Files-view dirs the user flipped away from their depth default. */
  toggled: ReadonlySet<string>;
}

/** Escape a string for literal use inside a RegExp. */
function escapeRegExp(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

// Map a path an agent reported to a right-pane file tab. A files-dir path
// (a report/figure the agent wrote under <data dir>/files/<slug>/…) is stripped
// to a files-dir-relative path and tagged source:"files" so FileViewer reads it
// from the /files endpoints. Otherwise it's a repo/worktree path stripped to
// repo-relative, keeping the session id when it points into a per-session
// worktree. Relative paths name files in the click context's checkout and
// inherit `contextSessionId`; the regex fallbacks encode the
// ~/.cache/openresearch/ layouts from src/local/git.rs:
// worktrees/<owner>/<repo>/<session>/… and repos/<owner>/<repo>/….
function parseFilePath(
  rawPath: string,
  repoPath?: string,
  contextSessionId?: string,
  filesDir?: string,
  slug?: string,
): FileViewDef | null {
  let path = rawPath;
  let sessionId: string | undefined;
  const clone = repoPath?.replace(/\/+$/, "");
  const files = filesDir?.replace(/\/+$/, "");
  if (!path.startsWith("/")) {
    sessionId = contextSessionId;
  } else if (files && (path === files || path.startsWith(`${files}/`))) {
    // Files-dir file — exact prefix match against the (non-canonical) dir the
    // backend surfaced, which mirrors what the agent inlines.
    const rel = path.slice(files.length).replace(/^\/+/, "");
    return rel ? { path: rel, source: "files" } : null;
  } else if (clone && (path === clone || path.startsWith(`${clone}/`))) {
    path = path.slice(clone.length).replace(/^\/+/, "");
  } else {
    // Files-dir fallback for a symlink-divergent path (e.g. /tmp vs
    // /private/tmp) where the exact prefix missed: match the …/files/<slug>/<rel>
    // layout, requiring the slug segment when we know it. (Legacy artifacts/ is
    // migrated to files/ in place, so it never appears in a live path.)
    const slugPat = slug ? escapeRegExp(slug) : "[^/]+";
    const fd = path.match(new RegExp(`/files/${slugPat}/(.+)$`));
    const wt = fd ? null : path.match(/\/openresearch\/worktrees\/[^/]+\/[^/]+\/([^/]+)\/(.+)$/);
    const hub = fd || wt ? null : path.match(/\/openresearch\/repos\/[^/]+\/[^/]+\/(.+)$/);
    if (fd) {
      return { path: fd[1], source: "files" };
    } else if (wt) {
      sessionId = wt[1];
      path = wt[2];
    } else if (hub) {
      path = hub[1];
    }
  }
  return path ? { path, sessionId } : null;
}

const ONBOARDED_KEY = "orx:onboarded";
const PANEL_WIDTH_KEY = "orx:panel-width";

/** Floating panel sizing: keep both the panel and the chat column usable. */
const PANEL_MIN_WIDTH = 360;
const PANEL_MARGIN = 10;
// Space the rest of the layout needs beside the panel: the ~232px rail, the
// chat column's minimum, and the gutters/margins between the three columns
// (app-body padding 14×2, rail inner margin 14, right-pane inner margin 14).
const RAIL_WIDTH = 232;
const CHAT_MIN_SPACE = 380;
const LAYOUT_CHROME = RAIL_WIDTH + 14 * 4;
// Once a drag pushes the panel past its usable max by this much, it snaps to
// fullscreen — a bit of resistance you have to overcome deliberately.
const FULLSCREEN_SNAP_SLOP = 80;

/** The widest the floating panel can be while leaving the rail + chat usable. */
function panelMaxWidth(): number {
  return Math.max(PANEL_MIN_WIDTH, window.innerWidth - LAYOUT_CHROME - CHAT_MIN_SPACE);
}

function initialPanelWidth(): number {
  const max = panelMaxWidth();
  try {
    const saved = Number(localStorage.getItem(PANEL_WIDTH_KEY));
    if (Number.isFinite(saved) && saved >= PANEL_MIN_WIDTH) return Math.min(saved, max);
  } catch {
    // storage unavailable — fall through to the default
  }
  return Math.max(PANEL_MIN_WIDTH, Math.min(760, max, Math.round(window.innerWidth * 0.42)));
}

function upsert<T extends { id: string }>(list: T[], item: T): T[] {
  const i = list.findIndex((x) => x.id === item.id);
  if (i < 0) return [...list, item];
  const next = list.slice();
  next[i] = item;
  return next;
}

export default function App() {
  const [projects, setProjects] = useState<Project[] | null>(null);
  const [projectId, setProjectId] = useState<string | null>(null);
  const [experiments, setExperiments] = useState<Experiment[]>([]);
  const [runs, setRuns] = useState<Run[]>([]);
  const [files, setFiles] = useState<ProjectFiles | null>(null);
  const [view, setView] = useState<"tree" | "table">("tree");
  const [selectedRunId, setSelectedRunId] = useState<string | null>(null);
  // Right-panel tab strip: the pinned Experiments tab plus a closable tab per
  // opened experiment view / project file. Views are single-purpose, so the
  // same experiment can hold both a terminal tab and a changes tab.
  const [rightTab, setRightTab] = useState<
    "experiments" | ExpViewDef | FileViewDef | PlanViewDef | CodeTabDef | WorktreeTabDef
  >("experiments");
  const [expTabs, setExpTabs] = useState<ExpViewDef[]>([]);
  const [fileTabs, setFileTabs] = useState<FileViewDef[]>([]);
  const [planTabs, setPlanTabs] = useState<PlanViewDef[]>([]);
  // At most one code-browser tab per project; null = not open.
  const [codeTab, setCodeTab] = useState<CodeTabDef | null>(null);
  // At most one live worktree tab, bound to the session it was opened from;
  // null = not open.
  const [worktreeTab, setWorktreeTab] = useState<WorktreeTabDef | null>(null);
  // The right pane is a floating panel: closable, edge-resizable, expandable
  // to (nearly) full screen. Width persists across sessions.
  const [panelOpen, setPanelOpen] = useState(true);
  const [panelMax, setPanelMax] = useState(false);
  const [panelWidth, setPanelWidth] = useState(initialPanelWidth);
  // The agents rail is a floating panel too: fixed-width, collapsible.
  const [railOpen, setRailOpen] = useState(true);
  const [homeOpen, setHomeOpen] = useState(false);
  // What the middle pane shows: the agent chat, the project's files, or
  // one settings section (picked from the rail nav — no separate pages).
  const [mainView, setMainView] = useState<"chat" | "files" | SettingsTab>("chat");
  const [onboarded, setOnboarded] = useState(() => {
    try {
      return localStorage.getItem(ONBOARDED_KEY) === "1";
    } catch {
      return true; // storage unavailable — don't loop the walkthrough
    }
  });
  // The spotlight tour of the workspace (Tour.tsx). Starting it normalizes
  // the layout so every tour target exists; those are the defaults, so
  // nothing needs restoring on finish/skip.
  const [tourOpen, setTourOpen] = useState(false);
  const startTour = useCallback(() => {
    setMainView("chat");
    setRailOpen(true);
    setPanelOpen(true);
    setPanelMax(false);
    setTourOpen(true);
  }, []);
  const closeTour = useCallback(() => {
    try {
      localStorage.setItem(TOUR_DONE_KEY, "1");
    } catch {
      // private mode etc. — the tour just replays next boot
    }
    setTourOpen(false);
  }, []);

  // Auto-start the tour the first time the workspace is actually on screen:
  // first-run walkthrough done, a project open, projects home closed. With
  // zero projects this waits until the first one is created and opened.
  useEffect(() => {
    if (!projectId || homeOpen || !onboarded) return;
    try {
      if (localStorage.getItem(TOUR_DONE_KEY) === "1") return;
    } catch {
      return; // storage unavailable — don't loop the tour
    }
    startTour();
  }, [projectId, homeOpen, onboarded, startTour]);

  const projectIdRef = useRef(projectId);
  projectIdRef.current = projectId;

  // Initial project list.
  useEffect(() => {
    listProjects()
      .then((list) => {
        setProjects(list);
        setProjectId((cur) => cur ?? list[0]?.id ?? null);
      })
      .catch(() => setProjects([]));
  }, []);

  // Shrinking the window can push a fixed-width panel past its usable max —
  // reclamp so it never overflows the viewport.
  useEffect(() => {
    const onResize = () => setPanelWidth((w) => Math.min(w, panelMaxWidth()));
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, []);

  // Per-project data. Harness agents spawn lazily on the first chat message.
  useEffect(() => {
    if (!projectId) return;
    // Record the visit; the resulting project.updated SSE event refreshes the
    // list's recency order.
    openProject(projectId).catch(() => {});
    setExperiments([]);
    setRuns([]);
    setFiles(null);
    setSelectedRunId(null);
    setExpTabs([]);
    setFileTabs([]);
    setPlanTabs([]);
    setCodeTab(null);
    setWorktreeTab(null);
    setRightTab("experiments");
    listExperiments(projectId).then(setExperiments).catch(() => {});
    listRuns(projectId).then(setRuns).catch(() => {});
    getFiles(projectId).then(setFiles).catch(() => {});
  }, [projectId]);

  // Refetch the files listing (on open and whenever the dir changes).
  const refreshFiles = useCallback(() => {
    const id = projectIdRef.current;
    if (id) getFiles(id).then(setFiles).catch(() => {});
  }, []);

  // Live store updates.
  useOrxEvents({
    onRun: (run) => {
      if (run.projectId === projectIdRef.current) setRuns((cur) => upsert(cur, run));
    },
    onExperiment: (experiment) => {
      if (experiment.projectId === projectIdRef.current)
        setExperiments((cur) => upsert(cur, experiment));
    },
    onProject: (project) => {
      setProjects((cur) => (cur ? upsert(cur, project) : [project]));
    },
    onFiles: (pid) => {
      if (pid === projectIdRef.current) refreshFiles();
    },
  });

  // Open an experiment view as a right-panel tab (creating it if needed) and
  // focus it.
  const openExperimentTab = useCallback((id: string, view: ExperimentView = "changes") => {
    const tab = { id, view };
    setExpTabs((prev) => (prev.some((t) => sameExpTab(t, tab)) ? prev : [...prev, tab]));
    setRightTab(tab);
    setPanelOpen(true);
  }, []);

  const closeExperimentTab = useCallback(
    (tab: ExpViewDef) => {
      const idx = expTabs.findIndex((t) => sameExpTab(t, tab));
      if (idx === -1) return;
      const next = expTabs.filter((_, i) => i !== idx);
      setExpTabs(next);
      // Closing the focused tab falls back to a neighbor, else the Log tab.
      if (typeof rightTab === "object" && "id" in rightTab && sameExpTab(rightTab, tab))
        setRightTab(next[Math.min(idx, next.length - 1)] ?? "experiments");
    },
    [expTabs, rightTab],
  );

  // Open a project file as a right-panel tab. `contextSessionId` is the chat
  // session (or viewed file's session) the click came from — see
  // parseFilePath for how it resolves against the reported path.
  const openFileTab = useCallback(
    (rawPath: string, contextSessionId?: string, ref?: string) => {
      const project = projects?.find((p) => p.id === projectId);
      const tab = parseFilePath(
        rawPath,
        project?.repoPath,
        contextSessionId,
        project?.filesDir,
        project?.slug,
      );
      if (!tab) return;
      // A branch ref only applies to repo files; never stamp it onto a
      // files-dir tab (it has no branch, and would fragment the tab identity).
      if (ref && tab.source !== "files") tab.ref = ref;
      setFileTabs((prev) => (prev.some((t) => sameFileTab(t, tab)) ? prev : [...prev, tab]));
      setRightTab(tab);
      setPanelOpen(true);
    },
    [projects, projectId],
  );

  const closeFileTab = useCallback(
    (tab: FileViewDef) => {
      const idx = fileTabs.findIndex((t) => sameFileTab(t, tab));
      if (idx === -1) return;
      const next = fileTabs.filter((_, i) => i !== idx);
      setFileTabs(next);
      if (typeof rightTab === "object" && "path" in rightTab && sameFileTab(rightTab, tab))
        setRightTab(next[Math.min(idx, next.length - 1)] ?? "experiments");
    },
    [fileTabs, rightTab],
  );

  // Open a proposed plan as a right-panel tab (the chat plan strip's "View
  // plan"). One tab per plan card; re-opening the same card refreshes its
  // text (a revised plan re-uses the strip but is a new promptId → new tab).
  const openPlanTab = useCallback((plan: string, sessionId: string, promptId: string) => {
    const tab: PlanViewDef = { kind: "plan", sessionId, promptId, plan };
    setPlanTabs((prev) => {
      const idx = prev.findIndex((t) => t.promptId === promptId);
      if (idx === -1) return [...prev, tab];
      const next = prev.slice();
      next[idx] = tab;
      return next;
    });
    setRightTab(tab);
    setPanelOpen(true);
  }, []);

  const closePlanTab = useCallback(
    (tab: PlanViewDef) => {
      const idx = planTabs.findIndex((t) => t.promptId === tab.promptId);
      if (idx === -1) return;
      const next = planTabs.filter((_, i) => i !== idx);
      setPlanTabs(next);
      if (typeof rightTab === "object" && "kind" in rightTab && rightTab.promptId === tab.promptId)
        setRightTab(next[Math.min(idx, next.length - 1)] ?? "experiments");
    },
    [planTabs, rightTab],
  );

  // Card shortcut: browse a specific experiment branch in the code tab.
  // Functional updater + no deps: a stable identity, so the TreeView cards
  // that receive this don't re-layout on every unrelated tab change.
  const openCodeTabForBranch = useCallback((branch: string) => {
    const opened: CodeTabDef = { code: true, sel: branch, toggled: new Set<string>() };
    setCodeTab((prev) => (prev ? { ...prev, sel: branch } : opened));
    // rightTab only discriminates on the `code` flag — the pane body always
    // renders the live `codeTab` state, so this value's other fields are
    // never read.
    setRightTab(opened);
    setPanelOpen(true);
  }, []);

  // Source/expansion changes persist on the tab def, not in CodeTab state —
  // the component unmounts whenever another right-pane tab fronts it.
  const updateCodeTab = useCallback((patch: Partial<Omit<CodeTabDef, "code">>) => {
    setCodeTab((prev) => (prev ? { ...prev, ...patch } : prev));
  }, []);

  const closeCodeTab = useCallback(() => {
    setCodeTab(null);
    setRightTab((cur) =>
      typeof cur === "object" && "code" in cur ? "experiments" : cur,
    );
  }, []);

  // Open (or re-front) the live worktree tab for a chat session — from the chat
  // header's worktree button. One tab at a time: opening it for a different
  // session rebinds it (Changes view, expansion state reset) rather than
  // stacking a second. Defaults to the Changes view.
  const openWorktreeTab = useCallback((sessionId: string) => {
    setWorktreeTab((prev) =>
      prev && prev.sessionId === sessionId
        ? prev
        : { wt: true, sessionId, view: "changes", toggled: new Set<string>() },
    );
    // rightTab only discriminates on the `wt` flag — the pane body always
    // renders the live `worktreeTab` state, so this value's fields aren't read.
    setRightTab({ wt: true, sessionId, view: "changes", toggled: new Set<string>() });
    setPanelOpen(true);
  }, []);

  // View/expansion changes persist on the tab def, not in WorktreeTab state —
  // the component unmounts whenever another right-pane tab fronts it.
  const updateWorktreeTab = useCallback((patch: Partial<Omit<WorktreeTabDef, "wt" | "sessionId">>) => {
    setWorktreeTab((prev) => (prev ? { ...prev, ...patch } : prev));
  }, []);

  const closeWorktreeTab = useCallback(() => {
    setWorktreeTab(null);
    setRightTab((cur) => (typeof cur === "object" && "wt" in cur ? "experiments" : cur));
  }, []);

  // A deleted session takes its worktree with it — close the tab rather than
  // leave it 404-ing over stale content (deletion arrives over SSE; ChatPanel
  // only forgets its own session list).
  useEffect(
    () =>
      onChatEvent((ev) => {
        if (ev.type !== "sessionDeleted") return;
        setWorktreeTab((prev) => {
          if (!prev || prev.sessionId !== ev.sessionId) return prev;
          setRightTab((cur) => (typeof cur === "object" && "wt" in cur ? "experiments" : cur));
          return null;
        });
      }),
    [],
  );

  // Drag the panel's left edge to resize; width persists across reloads.
  const resizePanel = (e: React.PointerEvent) => {
    e.preventDefault();
    // Capture the pointer so the terminal/diff views under the cursor don't
    // steal the drag, and suppress text selection for its duration.
    e.currentTarget.setPointerCapture(e.pointerId);
    const prevUserSelect = document.body.style.userSelect;
    document.body.style.userSelect = "none";
    const onMove = (ev: PointerEvent) => {
      const w = Math.round(window.innerWidth - ev.clientX - PANEL_MARGIN);
      const max = panelMaxWidth();
      // Drag past the usable max by the slop threshold → snap to fullscreen.
      // Dragging back below it drops out of fullscreen to the clamped width.
      if (w > max + FULLSCREEN_SNAP_SLOP) {
        setPanelMax(true);
        return;
      }
      setPanelMax(false);
      const clamped = Math.min(Math.max(w, PANEL_MIN_WIDTH), max);
      setPanelWidth(clamped);
      try {
        localStorage.setItem(PANEL_WIDTH_KEY, String(clamped));
      } catch {
        // best-effort persistence
      }
    };
    const stop = () => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", stop);
      window.removeEventListener("pointercancel", stop);
      document.body.style.userSelect = prevUserSelect;
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", stop);
    window.addEventListener("pointercancel", stop);
  };

  const onProjectCreated = (project: Project) => {
    setProjects((cur) => (cur ? upsert(cur, project) : [project]));
    setProjectId(project.id);
    setHomeOpen(false);
  };

  const onProjectDeleted = (id: string) => {
    setProjects((cur) => (cur ? cur.filter((p) => p.id !== id) : cur));
    if (projectId === id) setProjectId(null);
  };

  // ExpViewDef and WorktreeTabDef both carry a `view`; the experiment tab is
  // the one keyed by an experiment `id` (worktree tabs discriminate on `wt`).
  const expTab =
    typeof rightTab === "object" && "id" in rightTab ? rightTab : null;
  const fileTab = typeof rightTab === "object" && "path" in rightTab ? rightTab : null;
  const planTab = typeof rightTab === "object" && "kind" in rightTab ? rightTab : null;
  const codeTabActive = typeof rightTab === "object" && "code" in rightTab;
  const worktreeTabActive = typeof rightTab === "object" && "wt" in rightTab;
  const activeProject = projects?.find((p) => p.id === projectId) ?? null;
  const tabExperiment = expTab ? (experiments.find((e) => e.id === expTab.id) ?? null) : null;

  if (projects === null) {
    return (
      <div className="app">
        <div className="empty-state">
          <span className="spinner" />
        </div>
      </div>
    );
  }

  // First boot: agents walkthrough → git walkthrough → the (empty) projects
  // page, where the first project gets created.
  if (projects.length === 0) {
    return (
      <div className="app">
        {onboarded ? (
          <ProjectsHome
            projects={projects}
            onOpen={setProjectId}
            onCreated={onProjectCreated}
            onDeleted={onProjectDeleted}
          />
        ) : (
          <Onboarding
            onDone={() => {
              try {
                localStorage.setItem(ONBOARDED_KEY, "1");
              } catch {
                // private mode etc. — the flow just replays next boot
              }
              setOnboarded(true);
            }}
          />
        )}
      </div>
    );
  }

  const railHeader = (
    <RailHeader
      projectName={projects.find((p) => p.id === projectId)?.name ?? ""}
      onHome={() => setHomeOpen(true)}
      onCollapse={() => setRailOpen(false)}
    />
  );

  return (
    <div className="app">
      {homeOpen ? (
        <ProjectsHome
          projects={projects}
          onOpen={(id) => {
            setProjectId(id);
            setHomeOpen(false);
          }}
          onCreated={onProjectCreated}
          onDeleted={onProjectDeleted}
        />
      ) : (
      <div className="app-body">
        {projectId && (
          <ChatPanel
            projectId={projectId}
            paperId={projects.find((p) => p.id === projectId)?.paperId}
            railHeader={railHeader}
            railOpen={railOpen}
            onShowRail={() => setRailOpen(true)}
            mainView={mainView}
            onSelectMainView={setMainView}
            panelOpen={panelOpen}
            onTogglePanel={() => {
              if (panelOpen) setPanelMax(false);
              setPanelOpen(!panelOpen);
            }}
            onOpenFile={openFileTab}
            onOpenPlan={openPlanTab}
            onOpenWorktree={openWorktreeTab}
            onStartTour={startTour}
          >
            {mainView === "files" ? (
              (() => {
                const project = projects.find((p) => p.id === projectId);
                return project ? (
                  <FilesTab
                    // Remount per project: selection, collapsed-folder state,
                    // and the auto-select latch must not leak across projects.
                    key={project.id}
                    project={project}
                    files={files}
                    onChanged={refreshFiles}
                    onOpenStorage={() => setMainView("storage")}
                  />
                ) : null;
              })()
            ) : mainView !== "chat" ? (
              <SettingsView tab={mainView} />
            ) : null}
          </ChatPanel>
        )}
        {mainView === "chat" && panelOpen && (
        <aside
          className={`right-pane floating-panel ${panelMax ? "max" : ""}`}
          style={panelMax ? undefined : { width: panelWidth }}
          data-onboarding="experiments"
        >
          {!panelMax && <div className="panel-resizer" onPointerDown={resizePanel} />}
          <div className="tabs">
            <div className="tab-strip">
              <button
                className={`tab ${rightTab === "experiments" ? "active" : ""}`}
                onClick={() => setRightTab("experiments")}
              >
                Experiments
              </button>
              {expTabs.map((t) => {
                const exp = experiments.find((e) => e.id === t.id);
                return (
                  <ClosableTab
                    key={`${t.id}:${t.view}`}
                    active={expTab !== null && sameExpTab(expTab, t)}
                    label={exp ? exp.title || exp.slug : "…"}
                    icon={
                      t.view === "terminal" ? (
                        <Terminal size={12} style={{ flexShrink: 0 }} />
                      ) : (
                        <GitBranch size={12} style={{ flexShrink: 0 }} />
                      )
                    }
                    onSelect={() => setRightTab(t)}
                    onClose={() => closeExperimentTab(t)}
                  />
                );
              })}
              {fileTabs.map((t) => (
                <ClosableTab
                  key={`file:${fileTabKey(t)}`}
                  active={fileTab !== null && sameFileTab(fileTab, t)}
                  label={t.path.split("/").pop() || t.path}
                  icon={<FileCode size={12} style={{ flexShrink: 0 }} />}
                  onSelect={() => setRightTab(t)}
                  onClose={() => closeFileTab(t)}
                />
              ))}
              {planTabs.map((t) => (
                <ClosableTab
                  key={`plan:${t.promptId}`}
                  active={planTab !== null && planTab.promptId === t.promptId}
                  label="Plan"
                  icon={<ScrollText size={12} style={{ flexShrink: 0 }} />}
                  onSelect={() => setRightTab(t)}
                  onClose={() => closePlanTab(t)}
                />
              ))}
              {codeTab && (
                <ClosableTab
                  key="code"
                  active={codeTabActive}
                  label="Code"
                  icon={<FolderTree size={12} style={{ flexShrink: 0 }} />}
                  onSelect={() => setRightTab(codeTab)}
                  onClose={closeCodeTab}
                />
              )}
              {worktreeTab && (
                <ClosableTab
                  key="worktree"
                  active={worktreeTabActive}
                  label="Worktree"
                  icon={<FolderGit2 size={12} style={{ flexShrink: 0 }} />}
                  onSelect={() => setRightTab(worktreeTab)}
                  onClose={closeWorktreeTab}
                />
              )}
            </div>
            <div className="panel-controls">
              <button
                className="icon-btn"
                title={panelMax ? "Restore panel" : "Expand panel"}
                aria-label={panelMax ? "Restore panel" : "Expand panel"}
                onClick={() => setPanelMax((m) => !m)}
              >
                {panelMax ? <Minimize2 size={14} /> : <Maximize2 size={14} />}
              </button>
              <button
                className="icon-btn"
                title="Close panel"
                aria-label="Close panel"
                onClick={() => {
                  setPanelOpen(false);
                  setPanelMax(false);
                }}
              >
                <X size={14} />
              </button>
            </div>
          </div>
          {rightTab === "experiments" ? (
            <div className="tab-body">
              <div className="pane-toolbar">
                <div className="seg">
                  <button
                    className={view === "tree" ? "active" : ""}
                    onClick={() => setView("tree")}
                  >
                    Tree
                  </button>
                  <button
                    className={view === "table" ? "active" : ""}
                    onClick={() => setView("table")}
                  >
                    Table
                  </button>
                </div>
              </div>
              <div className="pane-content">
                {view === "tree" ? (
                  activeProject && (
                    <TreeView
                      experiments={experiments}
                      runs={runs}
                      project={activeProject}
                      onOpenView={openExperimentTab}
                      onOpenCodeBranch={openCodeTabForBranch}
                    />
                  )
                ) : (
                  <RunsTable
                    runs={runs}
                    experiments={experiments}
                    onOpen={(run) => {
                      setSelectedRunId(run.id);
                      openExperimentTab(run.experimentId, "terminal");
                    }}
                    onOpenChanges={(experimentId) => openExperimentTab(experimentId, "changes")}
                    onCancel={(runId) => void cancelRun(runId).catch(() => {})}
                  />
                )}
              </div>
            </div>
          ) : fileTab ? (
            <div className="tab-body">
              {projectId && (
                <FileViewer
                  key={fileTabKey(fileTab)}
                  projectId={projectId}
                  path={fileTab.path}
                  source={fileTab.source}
                  sessionId={fileTab.sessionId}
                  gitRef={fileTab.ref}
                  onOpenFile={openFileTab}
                />
              )}
            </div>
          ) : planTab ? (
            <div className="tab-body">
              {/* The plan markdown is already client-side — render directly,
                  file links resolve against the plan's session worktree. */}
              <div className="pane-content plan-tab-content">
                <Md
                  text={planTab.plan}
                  onOpenFile={(path) => openFileTab(path, planTab.sessionId)}
                />
              </div>
            </div>
          ) : codeTabActive ? (
            <div className="tab-body">
              {projectId && activeProject && codeTab && (
                <CodeTab
                  key="code"
                  projectId={projectId}
                  project={activeProject}
                  experiments={experiments}
                  sel={codeTab.sel}
                  toggled={codeTab.toggled}
                  onSelChange={(sel) => updateCodeTab({ sel })}
                  onToggledChange={(toggled) => updateCodeTab({ toggled })}
                  onOpenFile={openFileTab}
                />
              )}
            </div>
          ) : worktreeTabActive ? (
            <div className="tab-body">
              {projectId && worktreeTab && (
                <WorktreeTab
                  // Remount when the bound session changes — its data, poll
                  // subscription, and request-id guard must not carry over.
                  key={`wt:${worktreeTab.sessionId}`}
                  sessionId={worktreeTab.sessionId}
                  projectId={projectId}
                  view={worktreeTab.view}
                  toggled={worktreeTab.toggled}
                  onViewChange={(view) => updateWorktreeTab({ view })}
                  onToggledChange={(toggled) => updateWorktreeTab({ toggled })}
                  onOpenFile={openFileTab}
                />
              )}
            </div>
          ) : (
            <div className="tab-body">
              {expTab && tabExperiment && activeProject && (
                <DetailDrawer
                  key={`${expTab.id}:${expTab.view}`}
                  experiment={tabExperiment}
                  project={activeProject}
                  view={expTab.view}
                  runs={runs}
                  selectedRunId={selectedRunId}
                  onSelectRun={setSelectedRunId}
                />
              )}
            </div>
          )}
        </aside>
        )}
      </div>
      )}
      {tourOpen && !homeOpen && projectId && <Tour onClose={closeTour} />}
    </div>
  );
}
