import { FileCode, GitBranch, Terminal } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import {
  cancelRun,
  getArtifacts,
  getHfSettings,
  listExperiments,
  listProjects,
  listRuns,
  type Artifacts,
  type Experiment,
  type HfSettings,
  type Project,
  type Run,
} from "./api";
import { ArtifactsTab } from "./components/ArtifactsTab";
import { ChatPanel } from "./components/ChatPanel";
import { ClosableTab } from "./components/ClosableTab";
import { DetailDrawer, type ExperimentView } from "./components/DetailDrawer";
import { FileViewer } from "./components/FileViewer";
import { RailHeader } from "./components/Header";
import { Onboarding } from "./components/Onboarding";
import { ProjectsHome } from "./components/ProjectsHome";
import { RunsTable } from "./components/RunsTable";
import { SettingsPage, type SettingsTab } from "./components/SettingsPage";
import { TreeView } from "./components/TreeView";
import { useOrxEvents } from "./events";

/** An experiment view open as a right-pane tab. */
interface ExpTabDef {
  id: string;
  view: ExperimentView;
}

const sameTab = (a: ExpTabDef, b: ExpTabDef) => a.id === b.id && a.view === b.view;

/** A project file open as a right-pane tab (clicked in chat tool rows). */
interface FileTabDef {
  path: string;
}

const ONBOARDED_KEY = "orx:onboarded";

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
  const [artifacts, setArtifacts] = useState<Artifacts | null>(null);
  const [view, setView] = useState<"tree" | "table">("tree");
  const [selectedExpId, setSelectedExpId] = useState<string | null>(null);
  const [selectedRunId, setSelectedRunId] = useState<string | null>(null);
  // Right-pane tab strip: the static Log tab plus a closable tab per opened
  // experiment view. Views are single-purpose, so the same experiment can hold
  // both a terminal tab and a changes tab.
  const [rightTab, setRightTab] = useState<"log" | "artifacts" | ExpTabDef | FileTabDef>("log");
  const [expTabs, setExpTabs] = useState<ExpTabDef[]>([]);
  const [fileTabs, setFileTabs] = useState<FileTabDef[]>([]);
  const [homeOpen, setHomeOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [settingsTab, setSettingsTab] = useState<SettingsTab>("harnesses");
  const [hfSettings, setHfSettings] = useState<HfSettings | null>(null);
  const [hfLoading, setHfLoading] = useState(true);
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

  // HF token status, once on load; refetched via onHfSettingsUpdated after save.
  useEffect(() => {
    getHfSettings()
      .then(setHfSettings)
      .catch(() => {})
      .finally(() => setHfLoading(false));
  }, []);

  // Per-project data. Harness agents spawn lazily on the first chat message.
  useEffect(() => {
    if (!projectId) return;
    setExperiments([]);
    setRuns([]);
    setArtifacts(null);
    setSelectedExpId(null);
    setSelectedRunId(null);
    setExpTabs([]);
    setFileTabs([]);
    setRightTab("log");
    listExperiments(projectId).then(setExperiments).catch(() => {});
    listRuns(projectId).then(setRuns).catch(() => {});
    getArtifacts(projectId).then(setArtifacts).catch(() => {});
  }, [projectId]);

  // Refetch the artifacts listing (on open and whenever the dir changes).
  const refreshArtifacts = useCallback(() => {
    const id = projectIdRef.current;
    if (id) getArtifacts(id).then(setArtifacts).catch(() => {});
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
    onArtifacts: (pid) => {
      if (pid === projectIdRef.current) refreshArtifacts();
    },
  });

  // Open an experiment view as a right-pane tab (creating it if needed) and
  // focus it.
  const openExperimentTab = useCallback((id: string, view: ExperimentView = "changes") => {
    setSelectedExpId(id);
    const tab = { id, view };
    setExpTabs((prev) => (prev.some((t) => sameTab(t, tab)) ? prev : [...prev, tab]));
    setRightTab(tab);
  }, []);

  const closeExperimentTab = useCallback(
    (tab: ExpTabDef) => {
      const idx = expTabs.findIndex((t) => sameTab(t, tab));
      if (idx === -1) return;
      const next = expTabs.filter((_, i) => i !== idx);
      setExpTabs(next);
      if (typeof rightTab === "object" && "view" in rightTab && sameTab(rightTab, tab))
        setRightTab(next[Math.min(idx, next.length - 1)] ?? "log");
    },
    [expTabs, rightTab],
  );

  // Open a project file as a right-pane tab. Agents report absolute paths
  // inside the clone; strip the clone prefix so tabs and the API stay
  // repo-relative.
  const openFileTab = useCallback(
    (rawPath: string) => {
      let path = rawPath;
      const repoPath = projects?.find((p) => p.id === projectId)?.repoPath;
      if (repoPath && path.startsWith(repoPath)) {
        path = path.slice(repoPath.length).replace(/^\/+/, "");
      } else if (path.startsWith("/")) {
        // Fallback: the ~/.cache/openresearch/repos/<owner>/<repo>/ layout.
        const m = path.match(/\/repos\/[^/]+\/[^/]+\/(.+)$/);
        if (m) path = m[1];
      }
      if (!path) return;
      const tab = { path };
      setFileTabs((prev) => (prev.some((t) => t.path === path) ? prev : [...prev, tab]));
      setRightTab(tab);
    },
    [projects, projectId],
  );

  const closeFileTab = useCallback(
    (tab: FileTabDef) => {
      const idx = fileTabs.findIndex((t) => t.path === tab.path);
      if (idx === -1) return;
      const next = fileTabs.filter((_, i) => i !== idx);
      setFileTabs(next);
      if (typeof rightTab === "object" && "path" in rightTab && rightTab.path === tab.path)
        setRightTab(next[Math.min(idx, next.length - 1)] ?? "log");
    },
    [fileTabs, rightTab],
  );

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
      onHome={() => {
        setHomeOpen(true);
        setSettingsOpen(false);
      }}
      onOpenSettings={() => {
        setSettingsTab("harnesses");
        setSettingsOpen(true);
      }}
    />
  );

  return (
    <div className="app">
      {settingsOpen ? (
        <SettingsPage
          hfSettings={hfSettings}
          hfLoading={hfLoading}
          onHfSettingsUpdated={setHfSettings}
          onClose={() => setSettingsOpen(false)}
          initialTab={settingsTab}
        />
      ) : homeOpen ? (
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
            railHeader={railHeader}
            onOpenFile={openFileTab}
            onOpenCompute={() => {
              setSettingsTab("compute");
              setSettingsOpen(true);
            }}
          />
        )}
        <div className="right-pane">
          <div className="tabs">
            <div className="tab-strip">
              <button
                className={`tab ${rightTab === "log" ? "active" : ""}`}
                onClick={() => setRightTab("log")}
              >
                Log
              </button>
              <button
                className={`tab ${rightTab === "artifacts" ? "active" : ""}`}
                onClick={() => setRightTab("artifacts")}
              >
                Artifacts
                {artifacts && artifacts.entries.length > 0 && (
                  <span className="tab-count">{artifacts.entries.length}</span>
                )}
              </button>
              {expTabs.map((t) => {
                const exp = experiments.find((e) => e.id === t.id);
                return (
                  <ClosableTab
                    key={`${t.id}:${t.view}`}
                    active={expTab !== null && sameTab(expTab, t)}
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
                  key={`file:${t.path}`}
                  active={fileTab !== null && fileTab.path === t.path}
                  label={t.path.split("/").pop() || t.path}
                  icon={<FileCode size={12} style={{ flexShrink: 0 }} />}
                  onSelect={() => setRightTab(t)}
                  onClose={() => closeFileTab(t)}
                />
              ))}
            </div>
            <div style={{ fontSize: 13, fontWeight: 700, whiteSpace: "nowrap" }}>
              {projects.find((p) => p.id === projectId)?.name ?? ""}
            </div>
          </div>
          {rightTab === "log" ? (
            <div className="tab-body">
              <div className="seg-float">
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
          ) : rightTab === "artifacts" ? (
            <div className="tab-body">
              {(() => {
                const project = projects.find((p) => p.id === projectId);
                return project ? (
                  <ArtifactsTab
                    project={project}
                    artifacts={artifacts}
                    onChanged={refreshArtifacts}
                  />
                ) : null;
              })()}
            </div>
          ) : fileTab ? (
            <div className="tab-body">
              {projectId && (
                <FileViewer
                  key={fileTab.path}
                  projectId={projectId}
                  path={fileTab.path}
                  onOpenFile={openFileTab}
                />
              )}
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
        </div>
      </div>
      )}
    </div>
  );
}
