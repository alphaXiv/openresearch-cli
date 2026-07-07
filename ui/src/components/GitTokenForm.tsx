import { useState } from "react";
import { saveGitToken, type GitSettings } from "../api";

/** Paste-a-PAT fallback for GitHub access — validated server-side, stored in
 * the synced env file. Reports the refreshed git settings on success. */
export function GitTokenForm({ onSaved }: { onSaved: (g: GitSettings) => void }) {
  const [token, setToken] = useState("");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (saving || !token.trim()) return;
    setSaving(true);
    setError(null);
    try {
      onSaved(await saveGitToken(token.trim()));
      setToken("");
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSaving(false);
    }
  }

  return (
    <form className="onb-token-form" onSubmit={submit}>
      <input
        type="password"
        value={token}
        onChange={(e) => setToken(e.target.value)}
        placeholder="ghp_… personal access token"
        autoComplete="off"
      />
      <button type="submit" className="btn" disabled={saving || !token.trim()}>
        {saving ? "Checking…" : "Save"}
      </button>
      <a
        href="https://github.com/settings/tokens/new?scopes=repo,workflow&description=orx"
        target="_blank"
        rel="noreferrer"
      >
        Create a token ↗
      </a>
      {error && <div className="error">{error}</div>}
    </form>
  );
}
