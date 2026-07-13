import { Plus, Trash2 } from "lucide-react";
import { useState } from "react";
import { deleteProject, timeAgo, type Project } from "../api";
import { NewProjectForm } from "./NewProjectForm";

export function ProjectsHome({
  projects,
  onOpen,
  onCreated,
  onDeleted,
}: {
  projects: Project[];
  onOpen: (id: string) => void;
  onCreated: (project: Project) => void;
  onDeleted: (id: string) => void;
}) {
  const [modalOpen, setModalOpen] = useState(false);
  const [deleting, setDeleting] = useState<string | null>(null);

  async function onDelete(p: Project) {
    const ok = window.confirm(
      `Delete project "${p.name}"?\n\nIts experiments, runs and chats are removed from orx. ` +
        `The GitHub repo (${p.githubOwner}/${p.githubRepo}) is kept.`,
    );
    if (!ok) return;
    setDeleting(p.id);
    try {
      await deleteProject(p.id);
      onDeleted(p.id);
    } catch (err) {
      window.alert(err instanceof Error ? err.message : String(err));
    } finally {
      setDeleting(null);
    }
  }

  return (
    <div className="home">
      <div className="home-inner">
        <div className="home-brand">
          Open<span>Research</span>
        </div>
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
              <div
                key={p.id}
                className="project-card"
                role="button"
                tabIndex={0}
                onClick={() => onOpen(p.id)}
                onKeyDown={(e) => {
                  if (e.key === "Enter" || e.key === " ") onOpen(p.id);
                }}
              >
                <span className="name">{p.name}</span>
                <span className="repo mono">
                  {p.githubOwner}/{p.githubRepo} · {p.baselineBranch}
                </span>
                {p.paperId && <span className="paper mono">arXiv {p.paperId}</span>}
                <span className="time">created {timeAgo(p.createdAt)}</span>
                <button
                  className="project-delete"
                  title={`Delete ${p.name}`}
                  disabled={deleting === p.id}
                  onClick={(e) => {
                    e.stopPropagation();
                    onDelete(p);
                  }}
                >
                  <Trash2 size={14} />
                </button>
              </div>
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
