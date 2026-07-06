import { useCallback, useEffect, useRef, useState } from "react";
import {
  cancelRun,
  ensureAgent,
  getAgentStatus,
  listExperiments,
  listProjects,
  listRuns,
  type AgentStatus,
  type Experiment,
  type Project,
  type Run,
} from "./api";
import { ChatPanel } from "./components/ChatPanel";
import { DetailDrawer } from "./components/DetailDrawer";
import { Header } from "./components/Header";
import { NewProjectForm } from "./components/NewProjectForm";
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
  const [tab, setTab] = useState<"tree" | "runs">("tree");
  const [selectedExpId, setSelectedExpId] = useState<string | null>(null);
  const [selectedRunId, setSelectedRunId] = useState<string | null>(null);

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

  const onProjectCreated = (project: Project) => {
    setProjects((cur) => (cur ? upsert(cur, project) : [project]));
    setProjectId(project.id);
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
              Welcome to or<span style={{ color: "var(--accent)" }}>x</span>
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
        onSelectProject={setProjectId}
        onProjectCreated={onProjectCreated}
        agent={agent}
        agentPending={agentPending}
      />
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
            <button className={`tab ${tab === "tree" ? "active" : ""}`} onClick={() => setTab("tree")}>
              Tree
            </button>
            <button className={`tab ${tab === "runs" ? "active" : ""}`} onClick={() => setTab("runs")}>
              Runs
            </button>
          </div>
          <div className="tab-body">
            {tab === "tree" ? (
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
      </div>
    </div>
  );
}
