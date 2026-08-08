import { Plus, Trash2 } from "lucide-react";
import { Wordmark } from "./Wordmark";
import { useEffect, useRef, useState } from "react";
import { deleteProject, timeAgo, type Project } from "../api";
import { NewProjectForm } from "./NewProjectForm";

export function NewProjectDialog({
  onClose,
  onCreated,
}: {
  onClose: () => void;
  onCreated: (project: Project) => void;
}) {
  const dialogRef = useRef<HTMLDivElement>(null);
  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;

  useEffect(() => {
    const dialog = dialogRef.current;
    if (!dialog) return;
    const previousFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const focusable = () =>
      [...dialog.querySelectorAll<HTMLElement>(
        'button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), a[href], [tabindex]:not([tabindex="-1"])',
      )];
    (focusable()[0] ?? dialog).focus();

    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        onCloseRef.current();
        return;
      }
      if (
        event.key === "Enter" &&
        (event.metaKey || event.ctrlKey) &&
        !event.altKey &&
        event.shiftKey
      ) {
        event.preventDefault();
        event.stopPropagation();
        return;
      }
      if (event.key !== "Tab") return;
      const controls = focusable();
      if (controls.length === 0) {
        event.preventDefault();
        dialog.focus();
        return;
      }
      const first = controls[0];
      const last = controls[controls.length - 1];
      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      }
    };

    document.addEventListener("keydown", handleKeyDown, true);
    return () => {
      document.removeEventListener("keydown", handleKeyDown, true);
      previousFocus?.focus();
    };
  }, []);

  return (
    <div
      className="modal-backdrop"
      onClick={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <div
        ref={dialogRef}
        className="modal"
        role="dialog"
        aria-modal="true"
        aria-labelledby="new-project-dialog-title"
        tabIndex={-1}
      >
        <h2 id="new-project-dialog-title">New project</h2>
        <NewProjectForm onCancel={onClose} onCreated={onCreated} />
      </div>
    </div>
  );
}

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
        `The local folder (${p.path}) is kept.`,
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
          <Wordmark />
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
            [...projects].sort((a, b) => b.updatedAt - a.updatedAt).map((p) => (
              <div
                key={p.id}
                className="project-card"
              >
                <button
                  className="project-card-open"
                  aria-label={`Open ${p.name}`}
                  onClick={() => onOpen(p.id)}
                />
                <span className="name">{p.name}</span>
                <span className="project-card-sync mono">local Git</span>
                {p.paperId && <span className="paper mono">arXiv {p.paperId}</span>}
                <span className="time">created {timeAgo(p.createdAt)}</span>
                <button
                  className="project-delete"
                  data-tip={`Delete ${p.name}`}
                  data-tip-align="end"
                  aria-label={`Delete ${p.name}`}
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
        <NewProjectDialog
          onClose={() => setModalOpen(false)}
          onCreated={(project) => {
            setModalOpen(false);
            onCreated(project);
          }}
        />
      )}
    </div>
  );
}
