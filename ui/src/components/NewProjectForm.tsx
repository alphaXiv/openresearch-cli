import { useState } from "react";
import { createProject, type Project } from "../api";

/** owner/repo out of anything a user pastes: a full GitHub URL (https or ssh),
 * with or without .git, or the bare `owner/repo` shorthand. */
function parseRepo(input: string): { owner: string; repo: string } | null {
  const s = input
    .trim()
    .replace(/^git@github\.com:/i, "")
    .replace(/^https?:\/\/(www\.)?github\.com\//i, "")
    .replace(/\.git$/i, "")
    .replace(/^\/+|\/+$/g, "");
  const [owner, repo] = s.split("/");
  if (!owner || !repo || /[\s:@]/.test(owner) || /[\s:@]/.test(repo)) return null;
  return { owner, repo };
}

/** Mirror of the server's slugify — previews the repo name a blank project gets. */
function slugify(text: string): string {
  return (
    text
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, "-")
      .replace(/^-+|-+$/g, "")
      .slice(0, 48)
      .replace(/-+$/, "") || "experiment"
  );
}

type Mode = "existing" | "new";

export function NewProjectForm({
  onCreated,
  onCancel,
}: {
  onCreated: (project: Project) => void;
  onCancel?: () => void;
}) {
  const [mode, setMode] = useState<Mode>("existing");
  const [repoInput, setRepoInput] = useState("");
  const [name, setName] = useState("");
  const [nameTouched, setNameTouched] = useState(false);
  const [branch, setBranch] = useState("main");
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const parsed = parseRepo(repoInput);
  const valid = name.trim() && (mode === "new" || parsed !== null);

  const onRepoChange = (value: string) => {
    setRepoInput(value);
    // Name follows the repo until the user edits it themselves.
    if (!nameTouched) setName(parseRepo(value)?.repo ?? "");
  };

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (!valid || pending) return;
    setPending(true);
    setError(null);
    try {
      const project = await createProject(
        mode === "new"
          ? { name: name.trim(), createRepo: true }
          : {
              name: name.trim(),
              githubOwner: parsed!.owner,
              githubRepo: parsed!.repo,
              baselineBranch: branch.trim() || "main",
            },
      );
      onCreated(project);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setPending(false);
    }
  }

  return (
    <form className="form" onSubmit={submit}>
      <div className="seg form-seg">
        <button
          type="button"
          className={mode === "existing" ? "active" : ""}
          onClick={() => setMode("existing")}
        >
          Existing repo
        </button>
        <button
          type="button"
          className={mode === "new" ? "active" : ""}
          onClick={() => setMode("new")}
        >
          New blank repo
        </button>
      </div>

      {mode === "existing" ? (
        <>
          <label>
            GitHub repository
            <input
              value={repoInput}
              onChange={(e) => onRepoChange(e.target.value)}
              placeholder="https://github.com/karpathy/nanoGPT"
              autoFocus
              spellCheck={false}
            />
            <span className={`repo-hint mono ${parsed ? "ok" : ""}`}>
              {parsed
                ? `${parsed.owner} / ${parsed.repo}`
                : repoInput.trim()
                  ? "paste a GitHub URL or owner/repo"
                  : "URL or owner/repo — cloned with your git credentials"}
            </span>
          </label>
          <div className="row2">
            <label>
              Project name
              <input
                value={name}
                onChange={(e) => {
                  setNameTouched(true);
                  setName(e.target.value);
                }}
                placeholder="my-research"
              />
            </label>
            <label>
              Baseline branch
              <input
                value={branch}
                onChange={(e) => setBranch(e.target.value)}
                placeholder="main"
              />
            </label>
          </div>
        </>
      ) : (
        <label>
          Project name
          <input
            value={name}
            onChange={(e) => {
              setNameTouched(true);
              setName(e.target.value);
            }}
            placeholder="my-research"
            autoFocus
          />
          <span className={`repo-hint mono ${name.trim() ? "ok" : ""}`}>
            {name.trim()
              ? `creates github.com/you/${slugify(name)} · private`
              : "a blank private repo is created on your GitHub account"}
          </span>
        </label>
      )}

      {error && <div className="error">{error}</div>}
      <div className="actions">
        {onCancel && (
          <button type="button" className="btn ghost" onClick={onCancel}>
            Cancel
          </button>
        )}
        <button type="submit" className="btn primary" disabled={!valid || pending}>
          {pending
            ? mode === "new"
              ? "Creating repo…"
              : "Cloning repo…"
            : "Create project"}
        </button>
      </div>
    </form>
  );
}
