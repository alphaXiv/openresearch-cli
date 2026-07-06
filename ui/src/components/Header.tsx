import { Home, Settings } from "lucide-react";
import { useState } from "react";
import type { AgentStatus, HfSettings, Project } from "../api";
import { NewProjectForm } from "./NewProjectForm";
import { SettingsModal } from "./SettingsModal";

export function Header({
  projects,
  projectId,
  onSelectProject,
  onProjectCreated,
  onHome,
  agent,
  agentPending,
  hfSettings,
  hfLoading,
  onHfSettingsUpdated,
  onProjectUpdated,
}: {
  projects: Project[];
  projectId: string | null;
  onSelectProject: (id: string) => void;
  onProjectCreated: (project: Project) => void;
  onHome: () => void;
  agent: AgentStatus | null;
  agentPending: boolean;
  hfSettings: HfSettings | null;
  hfLoading: boolean;
  onHfSettingsUpdated: (settings: HfSettings) => void;
  onProjectUpdated: (project: Project) => void;
}) {
  const [modalOpen, setModalOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);

  const agentClass = agent?.running ? "on" : agentPending ? "starting" : "";
  const agentLabel = agent?.running
    ? `agent on :${agent.port ?? "?"}${agent.model ? ` · ${agent.model}` : ""}`
    : agentPending
      ? "agent starting…"
      : "agent off";

  const hfWarning =
    hfSettings !== null &&
    (!hfSettings.configured || !hfSettings.valid || hfSettings.jobsWrite === false);
  const gearTitle = hfWarning
    ? "Hugging Face token needs attention — HF Jobs launches will fail until fixed"
    : "Settings";

  return (
    <header className="header">
      <button className="icon-btn" title="Projects" aria-label="Projects" onClick={onHome}>
        <Home size={15} />
      </button>
      <button className="brand" onClick={onHome} title="Projects">
        Open<span>Research</span>
      </button>
      <span
        style={{
          fontFamily: "var(--mono)",
          fontSize: 10,
          color: "var(--muted)",
          border: "1px solid var(--border)",
          padding: "0 4px",
          textTransform: "uppercase",
        }}
      >
        orx
      </span>
      {projects.length > 0 && (
        <select value={projectId ?? ""} onChange={(e) => onSelectProject(e.target.value)}>
          {projects.map((p) => (
            <option key={p.id} value={p.id}>
              {p.name} · {p.githubOwner}/{p.githubRepo}
            </option>
          ))}
        </select>
      )}
      <button className="btn sm" onClick={() => setModalOpen(true)}>
        + New project
      </button>
      <div className="spacer" />
      <div className="agent-dot-wrap" title={agentLabel}>
        <span className={`agent-dot ${agentClass}`} />
        {agentLabel}
      </div>
      <button
        className="icon-btn"
        title={gearTitle}
        aria-label="Settings"
        onClick={() => setSettingsOpen(true)}
      >
        <Settings size={15} />
        {hfWarning && <span className="warn-dot" />}
      </button>

      {modalOpen && (
        <div className="modal-backdrop" onClick={() => setModalOpen(false)}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <h2>New project</h2>
            <NewProjectForm
              onCancel={() => setModalOpen(false)}
              onCreated={(p) => {
                setModalOpen(false);
                onProjectCreated(p);
              }}
            />
          </div>
        </div>
      )}

      {settingsOpen && (
        <SettingsModal
          settings={hfSettings}
          loading={hfLoading}
          onUpdated={onHfSettingsUpdated}
          project={projects.find((p) => p.id === projectId) ?? null}
          onProjectUpdated={onProjectUpdated}
          onClose={() => setSettingsOpen(false)}
        />
      )}
    </header>
  );
}
