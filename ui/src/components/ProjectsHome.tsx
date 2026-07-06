import { Plus } from "lucide-react";
import { useState } from "react";
import { timeAgo, type Project } from "../api";
import { NewProjectForm } from "./NewProjectForm";

export function ProjectsHome({
  projects,
  onOpen,
  onCreated,
}: {
  projects: Project[];
  onOpen: (id: string) => void;
  onCreated: (project: Project) => void;
}) {
  const [modalOpen, setModalOpen] = useState(false);

  return (
    <div className="home">
      <div className="home-inner">
        <div className="home-head">
          <h2>Projects</h2>
          <button className="btn sm" onClick={() => setModalOpen(true)}>
            <Plus size={13} /> New project
          </button>
        </div>
        <div className="home-list">
          {projects.length === 0 ? (
            <div className="changes-note">No projects yet — create one to get started.</div>
          ) : (
            projects.map((p) => (
              <button key={p.id} className="project-card" onClick={() => onOpen(p.id)}>
                <span className="name">{p.name}</span>
                <span className="repo mono">
                  {p.githubOwner}/{p.githubRepo} · {p.baselineBranch}
                </span>
                <span className="time">created {timeAgo(p.createdAt)}</span>
              </button>
            ))
          )}
        </div>
      </div>

      {modalOpen && (
        <div className="modal-backdrop" onClick={() => setModalOpen(false)}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <h2>New project</h2>
            <NewProjectForm
              onCancel={() => setModalOpen(false)}
              onCreated={(p) => {
                setModalOpen(false);
                onCreated(p);
              }}
            />
          </div>
        </div>
      )}
    </div>
  );
}
