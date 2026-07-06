import { useState } from "react";
import type { AgentStatus, HfSettings, Project } from "../api";
import { NewProjectForm } from "./NewProjectForm";
import { SettingsModal } from "./SettingsModal";

function GearIcon() {
  return (
    <svg width="15" height="15" viewBox="0 0 16 16" fill="currentColor" aria-hidden="true">
      <path d="M8 5.25A2.75 2.75 0 1 0 8 10.75 2.75 2.75 0 0 0 8 5.25Zm0 4.25A1.5 1.5 0 1 1 8 6.5a1.5 1.5 0 0 1 0 3Z" />
      <path d="M9.09 1.5c.42 0 .78.29.87.7l.24 1.06c.34.13.66.32.96.55l1.04-.33a.9.9 0 0 1 1.05.4l1.09 1.87a.9.9 0 0 1-.18 1.11l-.8.73a4.9 4.9 0 0 1 0 1.11l.8.73c.31.28.38.74.18 1.11l-1.09 1.88a.9.9 0 0 1-1.05.4l-1.04-.34c-.3.23-.62.42-.96.55l-.24 1.07a.9.9 0 0 1-.87.7H6.91a.9.9 0 0 1-.87-.7L5.8 13.03a4.94 4.94 0 0 1-.96-.55l-1.04.34a.9.9 0 0 1-1.05-.4L1.66 10.54a.9.9 0 0 1 .18-1.11l.8-.73a4.9 4.9 0 0 1 0-1.11l-.8-.73a.9.9 0 0 1-.18-1.11l1.09-1.87a.9.9 0 0 1 1.05-.4l1.04.33c.3-.23.62-.42.96-.55l.24-1.06a.9.9 0 0 1 .87-.7h2.18ZM8 4a4 4 0 1 1 0 8 4 4 0 0 1 0-8Z" />
    </svg>
  );
}

export function Header({
  projects,
  projectId,
  onSelectProject,
  onProjectCreated,
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
      <div className="brand">
        or<span>x</span>
      </div>
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
        <GearIcon />
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
