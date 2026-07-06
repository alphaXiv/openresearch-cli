import { useState } from "react";
import type { AgentStatus, Project } from "../api";
import { NewProjectForm } from "./NewProjectForm";

export function Header({
  projects,
  projectId,
  onSelectProject,
  onProjectCreated,
  agent,
  agentPending,
}: {
  projects: Project[];
  projectId: string | null;
  onSelectProject: (id: string) => void;
  onProjectCreated: (project: Project) => void;
  agent: AgentStatus | null;
  agentPending: boolean;
}) {
  const [modalOpen, setModalOpen] = useState(false);

  const agentClass = agent?.running ? "on" : agentPending ? "starting" : "";
  const agentLabel = agent?.running
    ? `agent on :${agent.port ?? "?"}${agent.model ? ` · ${agent.model}` : ""}`
    : agentPending
      ? "agent starting…"
      : "agent off";

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
    </header>
  );
}
