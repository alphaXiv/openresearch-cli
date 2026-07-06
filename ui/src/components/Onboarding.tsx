import { ArrowLeft, ArrowRight, Check, RefreshCw } from "lucide-react";
import { useEffect, useState } from "react";
import {
  getGitSettings,
  getHarnesses,
  modelLabel,
  type GitSettings,
  type Harness,
} from "../api";

/** First-run walkthrough: the detected coding agents, then the git/GitHub
 * model, then hand off to the (empty) projects page. Purely informative —
 * nothing here gates anything. */
export function Onboarding({ onDone }: { onDone: () => void }) {
  const [step, setStep] = useState<0 | 1>(0);
  const [harnesses, setHarnesses] = useState<Harness[] | null>(null);
  const [git, setGit] = useState<GitSettings | null>(null);
  const [checking, setChecking] = useState(false);

  const load = (refresh: boolean) => {
    setChecking(true);
    void Promise.allSettled([
      getHarnesses(refresh).then(setHarnesses),
      getGitSettings().then(setGit),
    ]).finally(() => setChecking(false));
  };
  useEffect(() => load(false), []);

  return (
    <div className="home onboarding">
      <div className="home-inner">
        {step === 0 ? (
          <>
            <div className="onb-eyebrow">
              Open<span>Research</span> · step 1 of 2
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
        ) : (
          <>
            <div className="onb-eyebrow">
              Open<span>Research</span> · step 2 of 2
            </div>
            <h2 className="onb-title">Git &amp; GitHub</h2>
            <p className="onb-sub">
              A project is a clone of one of your GitHub repos, made with your own git
              credentials. Every experiment becomes a branch pushed to that repo — compute jobs
              clone it from there.
            </p>
            <div className="onb-cards">
              <GitCard git={git} />
            </div>
            <div className="onb-actions">
              <button className="btn ghost" onClick={() => setStep(0)}>
                <ArrowLeft size={12} /> Back
              </button>
              <div style={{ flex: 1 }} />
              <button className="btn primary" onClick={onDone}>
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

function GitCard({ git }: { git: GitSettings | null }) {
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
            : git.githubTokenSource === "gh"
              ? "signed in via gh CLI"
              : "no gh login or GITHUB_TOKEN"}
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
        <div className="onb-card-meta">
          Private repos and branch pushes need GitHub access — run <code>gh auth login</code> or
          set <code>GITHUB_TOKEN</code>. SSH keys for github.com also work.
        </div>
      )}
    </div>
  );
}
