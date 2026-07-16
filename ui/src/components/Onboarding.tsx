import { ArrowLeft, ArrowRight, Check, Copy, RefreshCw } from "lucide-react";
import { useEffect, useState } from "react";
import {
  getDataDir,
  getGitSettings,
  getHarnesses,
  getTelemetry,
  modelLabel,
  setDataDir,
  recordTelemetryConsent,
  setTelemetry,
  type DataDirSettings,
  type GitSettings,
  type Harness,
  type TelemetrySettings,
} from "../api";
import { GitTokenForm } from "./GitTokenForm";

/** First-run walkthrough: the detected coding agents, then the git/GitHub
 * model, then the usage-analytics choice, then hand off to the (empty)
 * projects page. Purely informative — nothing here gates anything. */
export function Onboarding({ onDone }: { onDone: () => void }) {
  const [step, setStep] = useState<0 | 1 | 2 | 3>(0);
  const [harnesses, setHarnesses] = useState<Harness[] | null>(null);
  const [git, setGit] = useState<GitSettings | null>(null);
  const [dataDir, setDataDirState] = useState<DataDirSettings | null>(null);
  const [telemetry, setTelemetryState] = useState<TelemetrySettings | null>(null);
  const [checking, setChecking] = useState(false);

  const load = (refresh: boolean) => {
    setChecking(true);
    void Promise.allSettled([
      getHarnesses(refresh).then(setHarnesses),
      getGitSettings().then(setGit),
      getDataDir().then(setDataDirState),
      getTelemetry().then(setTelemetryState),
    ]).finally(() => setChecking(false));
  };
  useEffect(() => load(false), []);

  // Leaving step 3 → record the final consent decision once (agree or reject),
  // so every user who reaches the analytics step is counted, including those who
  // accept the default. Default to enabled if the setting hasn't loaded yet
  // (that's the default state shown). Best-effort — never block finishing.
  const finishOnboarding = () => {
    void recordTelemetryConsent(telemetry?.enabled ?? true).catch(() => {});
    onDone();
  };

  return (
    <div className="home onboarding">
      <div className="home-inner">
        {step === 0 ? (
          <>
            <div className="onb-eyebrow">
              Open<span>Research</span> · step 1 of 4
            </div>
            <h2 className="onb-title">Your coding agents</h2>
            <p className="onb-sub">
              orx found the agent CLIs on this machine and drives them directly — chat and
              autoresearch run on your own logins, no extra API keys.
            </p>
            <div className="onb-cards">
              {harnesses === null ? (
                <div className="onb-loading">
                  <span className="spinner" /> Detecting Claude Code, Codex, OpenCode…
                </div>
              ) : (
                harnesses.map((h) => <AgentCard key={h.id} h={h} />)
              )}
            </div>
            <div className="onb-actions">
              <button className="btn ghost" onClick={() => load(true)} disabled={checking}>
                <RefreshCw size={12} className={checking ? "spin" : ""} /> Re-check
              </button>
              <div style={{ flex: 1 }} />
              <button className="btn primary" onClick={() => setStep(1)}>
                Continue <ArrowRight size={13} />
              </button>
            </div>
          </>
        ) : step === 1 ? (
          <>
            <div className="onb-eyebrow">
              Open<span>Research</span> · step 2 of 4
            </div>
            <h2 className="onb-title">Git &amp; GitHub</h2>
            <p className="onb-sub">
              A project is a clone of one of your GitHub repos, made with your own git
              credentials. Every experiment becomes a branch pushed to that repo — compute jobs
              clone it from there.
            </p>
            <div className="onb-cards">
              <GitCard git={git} onUpdate={setGit} />
            </div>
            <div className="onb-actions">
              <button className="btn ghost" onClick={() => setStep(0)}>
                <ArrowLeft size={12} /> Back
              </button>
              <button className="btn ghost" onClick={() => load(false)} disabled={checking}>
                <RefreshCw size={12} className={checking ? "spin" : ""} /> Re-check
              </button>
              <div style={{ flex: 1 }} />
              <button className="btn primary" onClick={() => setStep(2)}>
                Continue <ArrowRight size={13} />
              </button>
            </div>
          </>
        ) : step === 2 ? (
          <>
            <div className="onb-eyebrow">
              Open<span>Research</span> · step 3 of 4
            </div>
            <h2 className="onb-title">Where orx keeps your data</h2>
            <p className="onb-sub">
              Your local database, run logs, artifacts, and chat attachments live in one folder on
              this machine. The default works for most people — change it if you&apos;d rather keep
              it on another disk.
            </p>
            <div className="onb-cards">
              <StorageCard dataDir={dataDir} onUpdate={setDataDirState} />
            </div>
            <div className="onb-actions">
              <button className="btn ghost" onClick={() => setStep(1)}>
                <ArrowLeft size={12} /> Back
              </button>
              <div style={{ flex: 1 }} />
              <button className="btn primary" onClick={() => setStep(3)}>
                Continue <ArrowRight size={13} />
              </button>
            </div>
          </>
        ) : (
          <>
            <div className="onb-eyebrow">
              Open<span>Research</span> · step 4 of 4
            </div>
            <h2 className="onb-title">Usage analytics</h2>
            <p className="onb-sub">
              orx can send anonymous usage analytics to help improve the tool. No code, prompts,
              file contents, or identifiers are ever sent — just a random per-install id, the
              command run, and your OS.
            </p>
            <div className="onb-cards">
              <TelemetryCard telemetry={telemetry} onUpdate={setTelemetryState} />
            </div>
            <div className="onb-actions">
              <button className="btn ghost" onClick={() => setStep(2)}>
                <ArrowLeft size={12} /> Back
              </button>
              <div style={{ flex: 1 }} />
              <button className="btn primary" onClick={finishOnboarding}>
                Create your first project <ArrowRight size={13} />
              </button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}

function agentBadge(h: Harness): { cls: string; label: string } {
  if (h.agentReady) return { cls: "st-done", label: "connected" };
  if (h.installed) return { cls: "st-starting", label: "not signed in" };
  return { cls: "st-idle", label: "not detected" };
}

function AgentCard({ h }: { h: Harness }) {
  const badge = agentBadge(h);
  const version = h.version?.replace(/\s*\(.*\)$/, "");
  return (
    <div className="onb-card">
      <div className="onb-card-head">
        <span className="onb-card-name">{h.name}</span>
        <span className={`status-badge ${badge.cls}`}>
          {h.agentReady ? <Check size={12} strokeWidth={3} /> : <span className="dot" />}
          {badge.label}
        </span>
      </div>
      {h.agentReady ? (
        <>
          <div className="onb-card-detail mono">
            {h.account ?? "API key"}
            {h.plan ? ` · ${h.plan}` : ""}
          </div>
          <div className="onb-card-meta">
            {[
              version,
              h.models.length > 0 &&
                `${h.models.length} model${h.models.length === 1 ? "" : "s"} — ${h.models
                  .slice(0, 3)
                  .map((m) => modelLabel(m.id))
                  .join(", ")}${h.models.length > 3 ? ", …" : ""}`,
            ]
              .filter(Boolean)
              .join(" · ")}
          </div>
        </>
      ) : (
        <div className="onb-card-meta">{h.agentNote?.replace(/`/g, "")}</div>
      )}
    </div>
  );
}

function GitCard({
  git,
  onUpdate,
}: {
  git: GitSettings | null;
  onUpdate: (g: GitSettings) => void;
}) {
  const [copied, setCopied] = useState(false);
  const copyCmd = () => {
    void navigator.clipboard.writeText("gh auth login").then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    });
  };
  if (git === null) {
    return (
      <div className="onb-loading">
        <span className="spinner" /> Checking git…
      </div>
    );
  }
  if (!git.gitVersion) {
    return (
      <div className="onb-card">
        <div className="onb-card-head">
          <span className="onb-card-name">git</span>
          <span className="status-badge st-failed">
            <span className="dot" /> not found
          </span>
        </div>
        <div className="onb-card-meta">Install git to clone projects, then re-open orx.</div>
      </div>
    );
  }
  const identity = [git.userName, git.userEmail && `<${git.userEmail}>`]
    .filter(Boolean)
    .join(" ");
  return (
    <div className="onb-card">
      <div className="onb-card-row">
        <span className="onb-card-name">git</span>
        <span className="onb-card-detail mono">
          {git.gitVersion.replace(/^git version /, "")}
          {identity ? ` · ${identity}` : ""}
        </span>
        <span className={`status-badge ${identity ? "st-done" : "st-starting"}`}>
          {identity ? <Check size={12} strokeWidth={3} /> : <span className="dot" />}
          {identity ? "ready" : "no identity"}
        </span>
      </div>
      <div className="onb-card-row">
        <span className="onb-card-name">GitHub</span>
        <span className="onb-card-detail mono">
          {git.githubTokenSource === "env"
            ? "token from GITHUB_TOKEN"
            : git.githubTokenSource === "stored"
              ? "token saved in orx"
              : git.githubTokenSource === "gh"
                ? "signed in via gh CLI"
                : "not connected"}
        </span>
        <span className={`status-badge ${git.githubTokenSource ? "st-done" : "st-starting"}`}>
          {git.githubTokenSource ? <Check size={12} strokeWidth={3} /> : <span className="dot" />}
          {git.githubTokenSource ? "ready" : "check"}
        </span>
      </div>
      {!identity && (
        <div className="onb-card-meta">
          Set <code>git config --global user.name / user.email</code> so the agent can commit.
        </div>
      )}
      {!git.githubTokenSource && (
        <div className="onb-gh-options">
          <div className="onb-card-meta">
            GitHub access is used to clone your repos and push experiment branches. Connect
            either way:
          </div>
          <div className="onb-gh-option">
            <span className="onb-gh-option-label">GitHub CLI</span>
            <div className="onb-gh-option-body">
              {git.ghInstalled ? (
                <>
                  <code className="onb-gh-cmd">gh auth login</code>
                  <button className="btn ghost" onClick={copyCmd}>
                    {copied ? <Check size={12} strokeWidth={3} /> : <Copy size={12} />}
                    {copied ? "Copied" : "Copy"}
                  </button>
                  <span className="onb-gh-hint">run in a terminal, then Re-check</span>
                </>
              ) : (
                <span className="onb-gh-hint">
                  install the GitHub CLI, run <code>gh auth login</code>, then Re-check
                </span>
              )}
            </div>
          </div>
          <div className="onb-gh-or">or</div>
          <div className="onb-gh-option">
            <span className="onb-gh-option-label">Paste a token</span>
            <GitTokenForm onSaved={onUpdate} />
          </div>
        </div>
      )}
    </div>
  );
}

function StorageCard({
  dataDir,
  onUpdate,
}: {
  dataDir: DataDirSettings | null;
  onUpdate: (d: DataDirSettings) => void;
}) {
  const [path, setPath] = useState("");
  const [editing, setEditing] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Seed the input once the current path loads.
  useEffect(() => {
    if (dataDir && !path) setPath(dataDir.current);
  }, [dataDir, path]);

  if (dataDir === null) {
    return (
      <div className="onb-loading">
        <span className="spinner" /> Checking storage…
      </div>
    );
  }

  // $ORX_DATA_DIR forces the path — nothing to choose here.
  if (dataDir.source === "env") {
    return (
      <div className="onb-card">
        <div className="onb-card-row">
          <span className="onb-card-name">Data folder</span>
          <span className="onb-card-detail mono">{dataDir.current}</span>
          <span className="status-badge st-done">
            <Check size={12} strokeWidth={3} /> pinned
          </span>
        </div>
        <div className="onb-card-meta">
          Set by the <code>ORX_DATA_DIR</code> environment variable.
        </div>
      </div>
    );
  }

  const trimmed = path.trim();
  const changed = trimmed !== "" && trimmed !== dataDir.current;

  async function save() {
    if (saving || !changed) return;
    setSaving(true);
    setError(null);
    try {
      onUpdate(await setDataDir(trimmed));
      setEditing(false);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSaving(false);
    }
  }

  return (
    <div className="onb-card">
      <div className="onb-card-row">
        <span className="onb-card-name">Data folder</span>
        <span className="onb-card-detail mono">{dataDir.current}</span>
        <span className={`status-badge ${dataDir.isDefault ? "st-idle" : "st-done"}`}>
          {dataDir.isDefault ? <span className="dot" /> : <Check size={12} strokeWidth={3} />}
          {dataDir.isDefault ? "default" : "custom"}
        </span>
      </div>
      {editing ? (
        <>
          <input
            className="mono"
            type="text"
            value={path}
            onChange={(e) => setPath(e.target.value)}
            placeholder="/absolute/path/to/openresearch"
            autoComplete="off"
            spellCheck={false}
            style={{ width: "100%", marginTop: 10 }}
          />
          {error && <div className="error" style={{ marginTop: 8 }}>{error}</div>}
          <div style={{ display: "flex", gap: 6, marginTop: 10 }}>
            <button className="btn primary" onClick={save} disabled={saving || !changed}>
              {saving ? "Saving…" : "Use this folder"}
            </button>
            <button
              className="btn ghost"
              onClick={() => {
                setEditing(false);
                setPath(dataDir.current);
                setError(null);
              }}
              disabled={saving}
            >
              Cancel
            </button>
          </div>
        </>
      ) : (
        <div className="onb-card-meta" style={{ display: "flex", gap: 10, alignItems: "center" }}>
          The default is fine for most setups.
          <button className="btn ghost" onClick={() => setEditing(true)}>
            Change…
          </button>
        </div>
      )}
    </div>
  );
}

function TelemetryCard({
  telemetry,
  onUpdate,
}: {
  telemetry: TelemetrySettings | null;
  onUpdate: (t: TelemetrySettings) => void;
}) {
  const [saving, setSaving] = useState(false);
  if (telemetry === null) {
    return (
      <div className="onb-loading">
        <span className="spinner" /> Checking analytics…
      </div>
    );
  }
  const on = telemetry.enabled;
  // A per-run override (e.g. `--no-telemetry`) that isn't the persisted setting:
  // the toggle writes the persisted flag, but this run stays off regardless.
  const overridden = !on && telemetry.reason !== null && telemetry.reason !== "disabled via `orx telemetry off`";
  const choose = (enabled: boolean) => {
    if (saving || enabled === on) return;
    setSaving(true);
    void setTelemetry(enabled)
      .then(onUpdate)
      .finally(() => setSaving(false));
  };
  return (
    <div className="onb-card">
      <div className="onb-card-head">
        <div>
          <div className="onb-card-name">Share anonymous usage analytics</div>
          <div className="onb-card-meta" style={{ marginTop: 2 }}>
            {on
              ? "On — helps prioritize what to build next."
              : overridden
                ? `Off — ${telemetry.reason}.`
                : "Off — you can turn it back on anytime."}
          </div>
        </div>
        <div style={{ display: "flex", gap: 6, flex: "none" }}>
          <button
            className={`btn ${on ? "primary" : "ghost"}`}
            onClick={() => choose(true)}
            disabled={saving}
            aria-pressed={on}
          >
            {on ? <Check size={12} strokeWidth={3} /> : null} On
          </button>
          <button
            className={`btn ${!on ? "primary" : "ghost"}`}
            onClick={() => choose(false)}
            disabled={saving}
            aria-pressed={!on}
          >
            {!on ? <Check size={12} strokeWidth={3} /> : null} Off
          </button>
        </div>
      </div>
      <div className="onb-card-meta" style={{ marginTop: 12 }}>
        Sent: a random per-install id, the command run, CLI version, and OS. Never sent: code,
        prompts, file contents, paths, or repo names. Change anytime in Settings or with{" "}
        <code>orx telemetry off</code>.
      </div>
      {overridden && (
        <div className="onb-card-meta" style={{ marginTop: 8 }}>
          Note: this run is off because of {telemetry.reason}, which overrides the saved choice.
        </div>
      )}
    </div>
  );
}
