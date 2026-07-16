import { FileCode, GitBranch, Maximize2, Minimize2, ScrollText, Terminal, X } from "lucide-react";
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
import { TreeView } from "./components/TreeView";
import { useOrxEvents } from "./events";

/** An experiment view open as a right-panel tab. */
interface ExpViewDef {
  id: string;
  view: ExperimentView;
}

const sameExpTab = (a: ExpViewDef, b: ExpViewDef) => a.id === b.id && a.view === b.view;

/** A project file open as a right-panel tab (clicked in chat tool rows). */
interface FileViewDef {
  path: string;
  /** Chat session whose worktree holds the file (absent → hub clone). */
  sessionId?: string;
}

const sameFileTab = (a: FileViewDef, b: FileViewDef) =>
  a.path === b.path && a.sessionId === b.sessionId;

const fileTabKey = (t: FileViewDef) => `${t.sessionId ?? ""}:${t.path}`;

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

// Map a path an agent reported to a repo-relative one — keeping the session
// id when it points into a per-session worktree, so the file is read from the
// right checkout. Relative paths name files in the click context's checkout
// and inherit `contextSessionId`; the regex fallbacks encode the
// ~/.cache/openresearch/ layouts from src/local/git.rs:
// worktrees/<owner>/<repo>/<session>/… and repos/<owner>/<repo>/….
function parseFilePath(
  rawPath: string,
  repoPath?: string,
  contextSessionId?: string,
): FileViewDef | null {
  let path = rawPath;
  let sessionId: string | undefined;
  const clone = repoPath?.replace(/\/+$/, "");
  if (!path.startsWith("/")) {
    sessionId = contextSessionId;
  } else if (clone && (path === clone || path.startsWith(`${clone}/`))) {
    path = path.slice(clone.length).replace(/^\/+/, "");
  } else {
    const wt = path.match(/\/openresearch\/worktrees\/[^/]+\/[^/]+\/([^/]+)\/(.+)$/);
    const hub = wt ? null : path.match(/\/openresearch\/repos\/[^/]+\/[^/]+\/(.+)$/);
    if (wt) {
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
  const [selectedExpId, setSelectedExpId] = useState<string | null>(null);
  const [selectedRunId, setSelectedRunId] = useState<string | null>(null);
  // Right-panel tab strip: the pinned Experiments tab plus a closable tab per
  // opened experiment view / project file. Views are single-purpose, so the
  // same experiment can hold both a terminal tab and a changes tab.
  const [rightTab, setRightTab] = useState<
    "experiments" | ExpViewDef | FileViewDef | PlanViewDef
  >("experiments");
  const [expTabs, setExpTabs] = useState<ExpViewDef[]>([]);
  const [fileTabs, setFileTabs] = useState<FileViewDef[]>([]);
  const [planTabs, setPlanTabs] = useState<PlanViewDef[]>([]);
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
    setSelectedExpId(null);
    setSelectedRunId(null);
    setExpTabs([]);
    setFileTabs([]);
    setPlanTabs([]);
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
    setSelectedExpId(id);
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
      if (typeof rightTab === "object" && "view" in rightTab && sameExpTab(rightTab, tab))
        setRightTab(next[Math.min(idx, next.length - 1)] ?? "experiments");
    },
    [expTabs, rightTab],
  );

  // Open a project file as a right-panel tab. `contextSessionId` is the chat
  // session (or viewed file's session) the click came from — see
  // parseFilePath for how it resolves against the reported path.
  const openFileTab = useCallback(
    (rawPath: string, contextSessionId?: string) => {
      const repoPath = projects?.find((p) => p.id === projectId)?.repoPath;
      const tab = parseFilePath(rawPath, repoPath, contextSessionId);
      if (!tab) return;
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

  const expTab = typeof rightTab === "object" && "view" in rightTab ? rightTab : null;
  const fileTab = typeof rightTab === "object" && "path" in rightTab ? rightTab : null;
  const planTab = typeof rightTab === "object" && "kind" in rightTab ? rightTab : null;
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
          >
            {mainView === "files" ? (
              (() => {
                const project = projects.find((p) => p.id === projectId);
                return project ? (
                  <FilesTab project={project} files={files} onChanged={refreshFiles} />
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
                    onSelect={() => {
                      setRightTab(t);
                      setSelectedExpId(t.id);
                    }}
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
                  <TreeView
                    experiments={experiments}
                    runs={runs}
                    selectedId={selectedExpId}
                    onSelect={(id) => {
                      setSelectedRunId(null);
                      setSelectedExpId(id);
                    }}
                    onOpenView={openExperimentTab}
                  />
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
                  sessionId={fileTab.sessionId}
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
          ) : (
            <div className="tab-body">
              {expTab && tabExperiment && (
                <DetailDrawer
                  key={`${expTab.id}:${expTab.view}`}
                  experiment={tabExperiment}
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
    </div>
  );
}
