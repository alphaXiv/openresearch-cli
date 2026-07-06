import { GitBranch } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import {
  cancelRun,
  ensureAgent,
  getAgentStatus,
  getHfSettings,
  listExperiments,
  listProjects,
  listRuns,
  type AgentStatus,
  type Experiment,
  type HfSettings,
  type Project,
  type Run,
} from "./api";
import { ChatPanel } from "./components/ChatPanel";
import { ClosableTab } from "./components/ClosableTab";
import { DetailDrawer } from "./components/DetailDrawer";
import { Header } from "./components/Header";
import { NewProjectForm } from "./components/NewProjectForm";
import { ProjectsHome } from "./components/ProjectsHome";
import { RunsTable } from "./components/RunsTable";
import { TreeView } from "./components/TreeView";
import { useOrxEvents } from "./events";

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
  const [agent, setAgent] = useState<AgentStatus | null>(null);
  const [agentPending, setAgentPending] = useState(false);
  const [view, setView] = useState<"tree" | "table">("tree");
  const [selectedExpId, setSelectedExpId] = useState<string | null>(null);
  const [selectedRunId, setSelectedRunId] = useState<string | null>(null);
  // Right-pane tab strip: the static Log tab plus a closable tab per opened
  // experiment (its detail view renders as tab content, not an overlay).
  const [rightTab, setRightTab] = useState<"log" | string>("log");
  const [expTabs, setExpTabs] = useState<string[]>([]);
  const [homeOpen, setHomeOpen] = useState(false);
  const [hfSettings, setHfSettings] = useState<HfSettings | null>(null);
  const [hfLoading, setHfLoading] = useState(true);

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

  const kickAgent = useCallback((pid: string) => {
    setAgentPending(true);
    ensureAgent(pid)
      .then((r) =>
        setAgent((cur) => ({ ...(cur ?? { running: false }), running: r.running, port: r.port, projectId: pid })),
      )
      .catch(() => {})
      .finally(() => setAgentPending(false));
  }, []);

  // Per-project data + agent bring-up.
  useEffect(() => {
    if (!projectId) return;
    setExperiments([]);
    setRuns([]);
    setSelectedExpId(null);
    setSelectedRunId(null);
    setExpTabs([]);
    setRightTab("log");
    listExperiments(projectId).then(setExperiments).catch(() => {});
    listRuns(projectId).then(setRuns).catch(() => {});
    kickAgent(projectId);
  }, [projectId, kickAgent]);

  // Agent status poll (covers spawn latency + crashes).
  useEffect(() => {
    let alive = true;
    const tick = () =>
      getAgentStatus()
        .then((s) => alive && setAgent(s))
        .catch(() => {});
    void tick();
    const t = setInterval(tick, 5000);
    return () => {
      alive = false;
      clearInterval(t);
    };
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
  });

  // Open an experiment's detail view as a right-pane tab (creating it if
  // needed) and focus it.
  const openExperimentTab = useCallback((id: string) => {
    setSelectedExpId(id);
    setExpTabs((prev) => (prev.includes(id) ? prev : [...prev, id]));
    setRightTab(id);
  }, []);

  const closeExperimentTab = useCallback(
    (id: string) => {
      const idx = expTabs.indexOf(id);
      const next = expTabs.filter((t) => t !== id);
      setExpTabs(next);
      if (rightTab === id) setRightTab(next[Math.min(idx, next.length - 1)] ?? "log");
    },
    [expTabs, rightTab],
  );

  const onProjectCreated = (project: Project) => {
    setProjects((cur) => (cur ? upsert(cur, project) : [project]));
    setProjectId(project.id);
    setHomeOpen(false);
  };

  const selectedExperiment = experiments.find((e) => e.id === selectedExpId) ?? null;
  const agentRunning = Boolean(agent?.running && agent.projectId === projectId);

  if (projects === null) {
    return (
      <div className="app">
        <div className="empty-state">
          <span className="spinner" />
        </div>
      </div>
    );
  }

  // First-boot: no projects → one centered create form.
  if (projects.length === 0) {
    return (
      <div className="app">
        <div className="empty-state">
          <div className="center-card">
            <h2>
              Open<span>Research</span>
            </h2>
            <p className="sub">
              Point at a GitHub repo to start local autoresearch. The repo is cloned locally;
              experiments become branches and runs execute on Hugging Face Jobs.
            </p>
            <NewProjectForm onCreated={onProjectCreated} />
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="app">
      <Header
        projects={projects}
        projectId={projectId}
        onSelectProject={(id) => {
          setProjectId(id);
          setHomeOpen(false);
        }}
        onProjectCreated={onProjectCreated}
        onHome={() => setHomeOpen(true)}
        agent={agent}
        agentPending={agentPending}
        hfSettings={hfSettings}
        hfLoading={hfLoading}
        onHfSettingsUpdated={setHfSettings}
        onProjectUpdated={(p) => setProjects((cur) => (cur ? upsert(cur, p) : [p]))}
      />
      {homeOpen ? (
        <ProjectsHome
          projects={projects}
          onOpen={(id) => {
            setProjectId(id);
            setHomeOpen(false);
          }}
          onCreated={onProjectCreated}
        />
      ) : (
      <div className="app-body">
        {projectId && (
          <ChatPanel
            projectId={projectId}
            agentRunning={agentRunning}
            onRetryAgent={() => kickAgent(projectId)}
          />
        )}
        <div className="right-pane">
          <div className="tabs">
            <button className="tab active">Log</button>
            <div className="spacer" style={{ flex: 1 }} />
            <div style={{ fontSize: 13, fontWeight: 700 }}>
              {projects.find((p) => p.id === projectId)?.name ?? ""}
            </div>
          </div>
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
                  setSelectedExpId(id);
                  setSelectedRunId(null);
                }}
              />
            ) : (
              <RunsTable
                runs={runs}
                experiments={experiments}
                onOpen={(run) => {
                  setSelectedExpId(run.experimentId);
                  setSelectedRunId(run.id);
                }}
                onCancel={(runId) => void cancelRun(runId).catch(() => {})}
              />
            )}
          </div>
          {selectedExperiment && (
            <DetailDrawer
              experiment={selectedExperiment}
              runs={runs}
              selectedRunId={selectedRunId}
              onSelectRun={setSelectedRunId}
              onClose={() => {
                setSelectedExpId(null);
                setSelectedRunId(null);
              }}
            />
          )}
        </div>
      </div>
      )}
    </div>
  );
}
