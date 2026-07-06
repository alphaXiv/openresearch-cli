import { useState } from "react";
import { createProject, type Project } from "../api";

export function NewProjectForm({
  onCreated,
  onCancel,
}: {
  onCreated: (project: Project) => void;
  onCancel?: () => void;
}) {
  const [name, setName] = useState("");
  const [owner, setOwner] = useState("");
  const [repo, setRepo] = useState("");
  const [branch, setBranch] = useState("main");
  const [runCommand, setRunCommand] = useState("");
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const valid = name.trim() && owner.trim() && repo.trim();

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (!valid || pending) return;
    setPending(true);
    setError(null);
    try {
      const project = await createProject({
        name: name.trim(),
        githubOwner: owner.trim(),
        githubRepo: repo.trim(),
        baselineBranch: branch.trim() || "main",
        runCommand: runCommand.trim() || undefined,
      });
      onCreated(project);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setPending(false);
    }
  }

  return (
    <form className="form" onSubmit={submit}>
      <label>
        Project name
        <input
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder="my-research"
          autoFocus
        />
      </label>
      <div className="row2">
        <label>
          GitHub owner
          <input value={owner} onChange={(e) => setOwner(e.target.value)} placeholder="octocat" />
        </label>
        <label>
          Repository
          <input value={repo} onChange={(e) => setRepo(e.target.value)} placeholder="nanogpt" />
        </label>
      </div>
      <div className="row2">
        <label>
          Baseline branch
          <input value={branch} onChange={(e) => setBranch(e.target.value)} placeholder="main" />
        </label>
        <label>
          Run command
          <input
            value={runCommand}
            onChange={(e) => setRunCommand(e.target.value)}
            placeholder="bash run.sh"
          />
        </label>
      </div>
      {error && <div className="error">{error}</div>}
      <div className="actions">
        {onCancel && (
          <button type="button" className="btn ghost" onClick={onCancel}>
            Cancel
          </button>
        )}
        <button type="submit" className="btn primary" disabled={!valid || pending}>
          {pending ? "Cloning repo…" : "Create project"}
        </button>
      </div>
    </form>
  );
}
