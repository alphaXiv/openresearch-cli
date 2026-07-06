import { useState } from "react";
import { saveHfToken, updateProject, type HfSettings, type HfTokenSource, type Project } from "../api";

const SOURCE_LABELS: Record<HfTokenSource, string> = {
  env: "HF_TOKEN env var",
  openresearchEnv: "~/.openresearch/env",
  hfCache: "~/.cache/huggingface/token (hf auth login)",
};

function JobsBadge({ settings }: { settings: HfSettings }) {
  if (!settings.configured) return null;
  if (!settings.valid) return <span className="badge err">invalid token</span>;
  if (settings.jobsWrite === true) return <span className="badge ok">jobs: write OK</span>;
  if (settings.jobsWrite === false)
    return <span className="badge err">no job.write permission</span>;
  return <span className="badge">jobs permission unknown</span>;
}

function ProjectSection({
  project,
  onUpdated,
}: {
  project: Project;
  onUpdated: (project: Project) => void;
}) {
  const [runCommand, setRunCommand] = useState(project.runCommand ?? "");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const trimmed = runCommand.trim();
  const unchanged = trimmed === (project.runCommand ?? "");

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (!trimmed || unchanged || saving) return;
    setSaving(true);
    setError(null);
    try {
      onUpdated(await updateProject(project.id, { runCommand: trimmed }));
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSaving(false);
    }
  }

  return (
    <div className="settings-section">
      <h3>Project</h3>
      <div className="kv">
        <span className="k">Name</span>
        <span className="v">{project.name}</span>
        <span className="k">Repo</span>
        <span className="v">
          {project.githubOwner}/{project.githubRepo}
        </span>
      </div>
      {!project.runCommand && (
        <p className="settings-note">
          Run command not set — experiments can&apos;t run without one.
        </p>
      )}
      <form className="form settings-form" onSubmit={submit}>
        <label>
          Run command
          <input
            className="mono"
            type="text"
            value={runCommand}
            onChange={(e) => setRunCommand(e.target.value)}
            placeholder="e.g. bash run.sh"
            autoComplete="off"
            spellCheck={false}
          />
        </label>
        {error && <div className="error">{error}</div>}
        <div className="actions">
          <button type="submit" className="btn primary" disabled={!trimmed || unchanged || saving}>
            {saving ? "Saving…" : "Save"}
          </button>
        </div>
      </form>
    </div>
  );
}

export function SettingsModal({
  settings,
  loading,
  onUpdated,
  project,
  onProjectUpdated,
  onClose,
}: {
  settings: HfSettings | null;
  loading: boolean;
  onUpdated: (settings: HfSettings) => void;
  project?: Project | null;
  onProjectUpdated?: (project: Project) => void;
  onClose: () => void;
}) {
  const [token, setToken] = useState("");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (!token.trim() || saving) return;
    setSaving(true);
    setError(null);
    try {
      const next = await saveHfToken(token.trim());
      onUpdated(next);
      setToken("");
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSaving(false);
    }
  }

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h2>Settings</h2>
        {project && (
          <ProjectSection project={project} onUpdated={(p) => onProjectUpdated?.(p)} />
        )}
        <div className="settings-section">
          <h3>Hugging Face token</h3>
          {loading && !settings ? (
            <div className="settings-loading">
              <span className="spinner" /> Loading status…
            </div>
          ) : settings ? (
            <>
              <div className="kv">
                <span className="k">Account</span>
                <span className="v">{settings.username ?? "—"}</span>
                <span className="k">Token</span>
                <span className="v">{settings.maskedToken ?? "—"}</span>
                <span className="k">Source</span>
                <span className="v">
                  {settings.source ? SOURCE_LABELS[settings.source] : "not configured"}
                </span>
                <span className="k">Jobs</span>
                <span className="v">
                  <JobsBadge settings={settings} />
                  {!settings.configured && "not configured"}
                </span>
              </div>
              {settings.source === "env" && (
                <p className="settings-note">
                  HF_TOKEN is set in the environment and overrides any token saved here.
                </p>
              )}
            </>
          ) : (
            <div className="settings-loading">Could not load Hugging Face status.</div>
          )}

          <form className="form settings-form" onSubmit={submit}>
            <label>
              New token
              <input
                type="password"
                value={token}
                onChange={(e) => setToken(e.target.value)}
                placeholder="hf_…"
                autoComplete="off"
              />
            </label>
            {error && <div className="error">{error}</div>}
            <div className="actions">
              <button type="button" className="btn ghost" onClick={onClose}>
                Close
              </button>
              <button type="submit" className="btn primary" disabled={!token.trim() || saving}>
                {saving ? "Validating…" : "Save"}
              </button>
            </div>
          </form>
        </div>
      </div>
    </div>
  );
}
