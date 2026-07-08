import {
  ArrowLeft,
  Blocks,
  Cpu,
  GitBranch,
  RefreshCw,
  SquareTerminal,
  Trash2,
} from "lucide-react";
import { useEffect, useState } from "react";
import {
  deleteEnvVar,
  getEnvVars,
  getGitSettings,
  getHarnesses,
  getK8sSettings,
  getModalSettings,
  getSshHosts,
  provisionModal,
  removeGitToken,
  saveGitSettings,
  saveHfToken,
  saveK8sSettings,
  setEnvVar,
  sshPreflight,
  type EnvVar,
  type GitSettings,
  type Harness,
  type HarnessId,
  type HfSettings,
  type HfTokenSource,
  type K8sSettings,
  type ModalSettings,
  type ModalTokenSource,
  type SshHost,
  type SshPreflight,
  modelLabel,
} from "../api";
import { GitTokenForm } from "./GitTokenForm";

export type SettingsTab = "harnesses" | "compute" | "environment" | "git";
type Tab = SettingsTab;

// --- harnesses ---------------------------------------------------------------

function harnessStatus(h: Harness): { cls: string; label: string } {
  if (h.authenticated && (h.installed || h.id === "codex"))
    return { cls: "ok", label: "Connected" };
  if (h.installed) return { cls: "", label: "Not signed in" };
  return { cls: "err", label: "Not detected" };
}

function AuthLabel({ h }: { h: Harness }) {
  if (!h.authMethod) return <>—</>;
  return <>{h.authMethod === "oauth" ? "OAuth (subscription login)" : "API key"}</>;
}

function HarnessesTab() {
  const [harnesses, setHarnesses] = useState<Harness[] | null>(null);
  const [active, setActive] = useState<HarnessId>("claude-code");
  const [refreshing, setRefreshing] = useState(false);

  const load = (refresh: boolean) => {
    setRefreshing(true);
    getHarnesses(refresh)
      .then(setHarnesses)
      .catch(() => {})
      .finally(() => setRefreshing(false));
  };
  useEffect(() => load(false), []);

  const h = harnesses?.find((x) => x.id === active);

  return (
    <>
      <h1>Harnesses</h1>
      <p className="settings-sub">
        Coding-agent setups detected on this machine. The research agent chat is served by
        OpenCode; Claude Code and Codex accounts surface their models in the composer's model
        picker.
      </p>
      <div className="harness-tabs">
        {(harnesses ?? []).map((x) => (
          <button
            key={x.id}
            className={x.id === active ? "active" : ""}
            onClick={() => setActive(x.id)}
          >
            {x.name}
            <span className={`harness-dot ${harnessStatus(x).cls}`} />
          </button>
        ))}
      </div>
      {!harnesses ? (
        <div className="settings-loading">
          <span className="spinner" /> Detecting harnesses…
        </div>
      ) : !h ? null : (
        <div className="settings-card">
          <div className="settings-card-head">
            <span className={`badge ${harnessStatus(h).cls}`}>{harnessStatus(h).label}</span>
            <div className="spacer" style={{ flex: 1 }} />
            <button className="btn sm" onClick={() => load(true)} disabled={refreshing}>
              <RefreshCw size={12} className={refreshing ? "spin" : ""} /> Refresh
            </button>
          </div>
          <div className="kv">
            <span className="k">Binary</span>
            <span className="v">{h.binPath ?? "not found on PATH"}</span>
            <span className="k">Version</span>
            <span className="v">{h.version ?? "—"}</span>
            <span className="k">Auth</span>
            <span className="v">
              <AuthLabel h={h} />
            </span>
            {h.account && (
              <>
                <span className="k">{h.id === "opencode" ? "Providers" : "Account"}</span>
                <span className="v">{h.account}</span>
              </>
            )}
            {h.org && (
              <>
                <span className="k">Org</span>
                <span className="v">{h.org}</span>
              </>
            )}
            {h.plan && (
              <>
                <span className="k">Plan</span>
                <span className="v">{h.plan}</span>
              </>
            )}
            <span className="k">Agent models</span>
            <span className="v">
              {h.models.length > 0
                ? `${h.models.length} available — ${h.models
                    .slice(0, 4)
                    .map((m) => modelLabel(m.id))
                    .join(", ")}${h.models.length > 4 ? ", …" : ""}`
                : "none"}
            </span>
          </div>
          {!h.agentReady && h.agentNote && <p className="settings-note">{h.agentNote}</p>}
        </div>
      )}
    </>
  );
}

// --- compute (kubernetes) -------------------------------------------------------

function K8sHealthBadge({ s }: { s: K8sSettings }) {
  if (!s.configured) return <span className="badge">not configured</span>;
  const p = s.preflight;
  if (!p.kubectlFound) return <span className="badge err">kubectl not found</span>;
  if (!p.reachable) return <span className="badge err">cluster unreachable</span>;
  if (!p.canCreateJobs) return <span className="badge err">no job-create permission</span>;
  return <span className="badge ok">connected</span>;
}

function K8sSection() {
  const [settings, setSettings] = useState<K8sSettings | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [context, setContext] = useState("");
  const [namespace, setNamespace] = useState("");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const apply = (s: K8sSettings) => {
    setSettings(s);
    setContext(s.context ?? "");
    setNamespace(s.namespace);
  };

  useEffect(() => {
    getK8sSettings()
      .then(apply)
      .catch((err) => setLoadError(err instanceof Error ? err.message : String(err)));
  }, []);

  const unchanged =
    settings !== null &&
    context === (settings.context ?? "") &&
    namespace.trim() === settings.namespace;

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (saving) return;
    setSaving(true);
    setError(null);
    try {
      apply(await saveK8sSettings({ context, namespace: namespace.trim() }));
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSaving(false);
    }
  }

  return (
    <>
      <p className="settings-sub">
        <strong>Kubernetes</strong> — run on your own cluster with <code>--backend k8s</code>.
        The run&apos;s resources (image, GPUs, topology) come from a manifest committed on the
        experiment branch (default <code>.orx/k8s.yaml</code>); only the cluster context and
        namespace live here. Auth comes from your kubeconfig.
      </p>
      {loadError ? (
        <div className="settings-card">
          <div className="error">{loadError}</div>
        </div>
      ) : !settings ? (
        <div className="settings-loading">
          <span className="spinner" /> Checking kubectl…
        </div>
      ) : (
        <>
          <div className="settings-card">
            <div className="settings-card-head">
              <h3>Kubernetes cluster</h3>
              <div className="spacer" style={{ flex: 1 }} />
              <K8sHealthBadge s={settings} />
            </div>
            {settings.preflight.error && (
              <p className="settings-note">{settings.preflight.error}</p>
            )}
            <form className="form settings-form" onSubmit={submit}>
              <div className="row2">
                <label>
                  Context
                  <select value={context} onChange={(e) => setContext(e.target.value)}>
                    <option value="">
                      kubectl default{settings.currentContext ? ` (${settings.currentContext})` : ""}
                    </option>
                    {settings.contexts.map((c) => (
                      <option key={c} value={c}>
                        {c}
                      </option>
                    ))}
                  </select>
                </label>
                <label>
                  Namespace
                  <input
                    className="mono"
                    type="text"
                    value={namespace}
                    onChange={(e) => setNamespace(e.target.value)}
                    placeholder="default"
                    autoComplete="off"
                    spellCheck={false}
                  />
                </label>
              </div>
              {error && <div className="error">{error}</div>}
              <div className="actions">
                <button type="submit" className="btn primary" disabled={saving || unchanged}>
                  {saving ? "Saving…" : "Save"}
                </button>
              </div>
            </form>
          </div>
          <div className="settings-card">
            <div className="settings-card-head">
              <h3>Run manifest</h3>
            </div>
            <p className="settings-sub">
              Each run applies the manifest committed on its experiment branch — default{" "}
              <code>.orx/k8s.yaml</code>, or <code>--manifest &lt;path&gt;</code>. It declares
              whatever the run needs (image, GPU requests, an Indexed Job across nodes, extra
              Services, …); orx injects the run script as <code>$ORX_SCRIPT</code>, the{" "}
              <code>orx-env</code> Secret, run labels, and a default timeout, and requires
              exactly one Job (or one labelled <code>orx-primary: &quot;true&quot;</code>) whose
              completion is the run&apos;s. Logs follow that Job&apos;s leader pod. Use{" "}
              <code>{"{{ORX_RUN}}"}</code> in resource names to keep re-runs collision-free.
            </p>
          </div>
        </>
      )}
    </>
  );
}

// --- compute (modal) ------------------------------------------------------------

const MODAL_TOKEN_LABELS: Record<ModalTokenSource, string> = {
  env: "MODAL_TOKEN_ID env var",
  syncedEnv: "~/.openresearch/env",
  modalToml: "~/.modal.toml (modal token new)",
};

function ModalBadge({ s }: { s: ModalSettings }) {
  if (s.ready) return <span className="badge ok">connected</span>;
  if (!s.tokenConfigured && !s.modalImportable) return <span className="badge">not set up</span>;
  if (!s.modalImportable)
    return <span className="badge err">{s.envProvisioned ? "env broken" : "env not built"}</span>;
  if (!s.tokenConfigured) return <span className="badge err">no token</span>;
  return <span className="badge">unknown</span>;
}

function ModalSection() {
  const [s, setS] = useState<ModalSettings | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [provisioning, setProvisioning] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    getModalSettings()
      .then(setS)
      .catch((err) => setLoadError(err instanceof Error ? err.message : String(err)));
  }, []);

  async function provision() {
    if (provisioning) return;
    setProvisioning(true);
    setError(null);
    try {
      setS(await provisionModal());
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setProvisioning(false);
    }
  }

  return (
    <div className="settings-card">
      <div className="settings-card-head">
        <h3>Modal</h3>
        <div className="spacer" style={{ flex: 1 }} />
        {s && <ModalBadge s={s} />}
      </div>
      <p className="settings-sub">
        Serverless GPUs on your own Modal account with{" "}
        <code>--backend modal --flavor &lt;name&gt;</code> (t4, a10g, a100-80gb, h100, …). orx
        manages a dedicated Python env with the Modal SDK; sandboxes scale to zero between runs.
      </p>
      {loadError ? (
        <div className="error">{loadError}</div>
      ) : !s ? (
        <div className="settings-loading">
          <span className="spinner" /> Checking Modal…
        </div>
      ) : (
        <>
          <div className="kv">
            <span className="k">Environment</span>
            <span className="v">
              {s.modalImportable
                ? "ready"
                : s.envProvisioned
                  ? "provisioned (modal import failing)"
                  : "not built yet"}
            </span>
            <span className="k">Token</span>
            <span className="v">
              {s.tokenSource ? MODAL_TOKEN_LABELS[s.tokenSource] : "not configured"}
            </span>
          </div>
          {!s.tokenConfigured && (
            <p className="settings-note">
              No Modal token found. Run <code>modal token new</code>, or add{" "}
              <code>MODAL_TOKEN_ID</code> and <code>MODAL_TOKEN_SECRET</code> in the Environment
              tab.
            </p>
          )}
          {s.error && s.envProvisioned && !s.modalImportable && (
            <p className="settings-note">{s.error}</p>
          )}
          {error && <div className="error">{error}</div>}
          {!s.modalImportable && (
            <div className="actions">
              <button className="btn primary" onClick={() => void provision()} disabled={provisioning}>
                {provisioning ? "Setting up… (~30–60s)" : "Set up environment"}
              </button>
            </div>
          )}
        </>
      )}
    </div>
  );
}

// --- compute (ssh) ---------------------------------------------------------------

type HostTest = "testing" | SshPreflight;

function HostTestCell({ test }: { test: HostTest | undefined }) {
  if (test === undefined) return <span className="muted">—</span>;
  if (test === "testing") return <span className="spinner" />;
  if (!test.reachable)
    return <span className="badge err" title={test.error ?? undefined}>unreachable</span>;
  if (!test.gitFound) return <span className="badge err">no git</span>;
  return <span className="badge ok">ready</span>;
}

function SshSection() {
  const [hosts, setHosts] = useState<SshHost[] | null>(null);
  const [tests, setTests] = useState<Record<string, HostTest>>({});

  useEffect(() => {
    getSshHosts()
      .then(setHosts)
      .catch(() => setHosts([]));
  }, []);

  async function test(host: string) {
    setTests((t) => ({ ...t, [host]: "testing" }));
    try {
      const r = await sshPreflight(host);
      setTests((t) => ({ ...t, [host]: r }));
    } catch (err) {
      setTests((t) => ({
        ...t,
        [host]: { reachable: false, gitFound: false, error: err instanceof Error ? err.message : String(err) },
      }));
    }
  }

  return (
    <div className="settings-card">
      <h3>SSH hosts</h3>
      <p className="settings-sub">
        Run experiments directly on your own boxes with{" "}
        <code>--backend ssh --host &lt;alias&gt;</code>. Hosts come from{" "}
        <code>~/.ssh/config</code>; auth uses your keys/agent (orx never reads a key). The host
        just needs <code>git</code> and <code>bash</code>.
      </p>
      {hosts === null ? (
        <div className="settings-loading">
          <span className="spinner" /> Reading ~/.ssh/config…
        </div>
      ) : hosts.length === 0 ? (
        <p className="settings-empty">No hosts found in ~/.ssh/config.</p>
      ) : (
        <table className="flavor-table">
          <thead>
            <tr>
              <th>Host</th>
              <th>Address</th>
              <th>Identity</th>
              <th>Status</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {hosts.map((h) => (
              <tr key={h.host}>
                <td className="mono">{h.host}</td>
                <td className="mono muted">
                  {[h.user, h.hostname ?? "—"].filter(Boolean).join("@")}
                  {h.port ? `:${h.port}` : ""}
                </td>
                <td className="mono muted">{h.identityFile ?? "—"}</td>
                <td>
                  <HostTestCell test={tests[h.host]} />
                </td>
                <td>
                  <button
                    className="btn sm"
                    onClick={() => void test(h.host)}
                    disabled={tests[h.host] === "testing"}
                  >
                    Test
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

// --- compute -----------------------------------------------------------------

type ComputeSub = "hf" | "modal" | "k8s" | "ssh";

const COMPUTE_SUBS: { id: ComputeSub; label: string }[] = [
  { id: "hf", label: "HF Jobs" },
  { id: "modal", label: "Modal" },
  { id: "k8s", label: "Kubernetes" },
  { id: "ssh", label: "SSH" },
];

function ComputeTab({
  hfSettings,
  hfLoading,
  onHfSettingsUpdated,
}: {
  hfSettings: HfSettings | null;
  hfLoading: boolean;
  onHfSettingsUpdated: (settings: HfSettings) => void;
}) {
  const [sub, setSub] = useState<ComputeSub>("hf");
  return (
    <>
      <h1>Compute</h1>
      <p className="settings-sub">
        Run experiments on external compute with{" "}
        <code>--backend &lt;name&gt; --flavor &lt;name&gt;</code>.
      </p>
      <div className="harness-tabs">
        {COMPUTE_SUBS.map((t) => (
          <button
            key={t.id}
            className={t.id === sub ? "active" : ""}
            onClick={() => setSub(t.id)}
          >
            {t.label}
          </button>
        ))}
      </div>
      {sub === "hf" && (
        <HfSection settings={hfSettings} loading={hfLoading} onUpdated={onHfSettingsUpdated} />
      )}
      {sub === "modal" && <ModalSection />}
      {sub === "k8s" && <K8sSection />}
      {sub === "ssh" && <SshSection />}
    </>
  );
}

// --- environment ---------------------------------------------------------------

const SOURCE_LABELS: Record<HfTokenSource, string> = {
  env: "HF_TOKEN env var",
  openresearchEnv: "~/.openresearch/env",
  hfCache: "~/.cache/huggingface/token (hf auth login)",
};

function HfJobsBadge({ settings }: { settings: HfSettings }) {
  if (!settings.configured) return null;
  if (!settings.valid) return <span className="badge err">invalid token</span>;
  if (settings.jobsWrite === true) return <span className="badge ok">jobs: write OK</span>;
  if (settings.jobsWrite === false)
    return <span className="badge err">no job.write permission</span>;
  return <span className="badge">jobs permission unknown</span>;
}

function HfSection({
  settings,
  loading,
  onUpdated,
}: {
  settings: HfSettings | null;
  loading: boolean;
  onUpdated: (settings: HfSettings) => void;
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
      onUpdated(await saveHfToken(token.trim()));
      setToken("");
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSaving(false);
    }
  }

  return (
    <div className="settings-card">
      <h3>Hugging Face Jobs</h3>
      <p className="settings-sub">
        Run experiments on your Hugging Face account with{" "}
        <code>--backend hf --flavor &lt;name&gt;</code> (t4-small, a10g-small, a100-large, …).
        Billed to HF per minute.
      </p>
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
              <HfJobsBadge settings={settings} />
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
          <button type="submit" className="btn primary" disabled={!token.trim() || saving}>
            {saving ? "Validating…" : "Save"}
          </button>
        </div>
      </form>
    </div>
  );
}

function EnvVarsSection() {
  const [vars, setVars] = useState<EnvVar[] | null>(null);
  const [key, setKey] = useState("");
  const [value, setValue] = useState("");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    getEnvVars()
      .then(setVars)
      .catch(() => setVars([]));
  }, []);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (!key.trim() || !value.trim() || saving) return;
    setSaving(true);
    setError(null);
    try {
      setVars(await setEnvVar(key.trim(), value.trim()));
      setKey("");
      setValue("");
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSaving(false);
    }
  }

  async function remove(k: string) {
    try {
      setVars(await deleteEnvVar(k));
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  return (
    <div className="settings-card">
      <h3>Environment variables</h3>
      <p className="settings-sub">
        Stored in <code>~/.openresearch/env</code> and passed to the research agent (API keys,
        tokens). Variables set in orx's own environment win on conflicts.
      </p>
      {vars === null ? (
        <div className="settings-loading">
          <span className="spinner" /> Loading…
        </div>
      ) : vars.length === 0 ? (
        <p className="settings-empty">No variables saved yet.</p>
      ) : (
        <table className="env-table">
          <tbody>
            {vars.map((v) => (
              <tr key={v.key}>
                <td className="mono">{v.key}</td>
                <td className="mono muted">{v.maskedValue}</td>
                <td>{v.inProcessEnv && <span className="badge">overridden by env</span>}</td>
                <td>
                  <button
                    className="icon-btn"
                    title={`Delete ${v.key}`}
                    aria-label={`Delete ${v.key}`}
                    onClick={() => void remove(v.key)}
                  >
                    <Trash2 size={13} />
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      <form className="form settings-form" onSubmit={submit}>
        <div className="row2">
          <label>
            Key
            <input
              className="mono"
              type="text"
              value={key}
              onChange={(e) => setKey(e.target.value)}
              placeholder="WANDB_API_KEY"
              autoComplete="off"
              spellCheck={false}
            />
          </label>
          <label>
            Value
            <input
              className="mono"
              type="password"
              value={value}
              onChange={(e) => setValue(e.target.value)}
              placeholder="value"
              autoComplete="off"
            />
          </label>
        </div>
        {error && <div className="error">{error}</div>}
        <div className="actions">
          <button
            type="submit"
            className="btn primary"
            disabled={!key.trim() || !value.trim() || saving}
          >
            {saving ? "Saving…" : "Add variable"}
          </button>
        </div>
      </form>
    </div>
  );
}

// --- git -----------------------------------------------------------------------

function GitTab() {
  const [settings, setSettings] = useState<GitSettings | null>(null);
  const [name, setName] = useState("");
  const [email, setEmail] = useState("");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    getGitSettings()
      .then((s) => {
        setSettings(s);
        setName(s.userName ?? "");
        setEmail(s.userEmail ?? "");
      })
      .catch(() => {});
  }, []);

  const unchanged =
    settings !== null &&
    name.trim() === (settings.userName ?? "") &&
    email.trim() === (settings.userEmail ?? "");

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (saving || unchanged) return;
    setSaving(true);
    setError(null);
    try {
      const next = await saveGitSettings({ userName: name.trim(), userEmail: email.trim() });
      setSettings(next);
      setName(next.userName ?? "");
      setEmail(next.userEmail ?? "");
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSaving(false);
    }
  }

  return (
    <>
      <h1>Git</h1>
      <p className="settings-sub">
        Experiment branches are committed and pushed from local clones with this identity.
      </p>
      {!settings ? (
        <div className="settings-loading">
          <span className="spinner" /> Loading…
        </div>
      ) : (
        <>
          <div className="settings-card">
            <h3>Identity</h3>
            <form className="form" onSubmit={submit}>
              <div className="row2">
                <label>
                  user.name
                  <input
                    type="text"
                    value={name}
                    onChange={(e) => setName(e.target.value)}
                    autoComplete="off"
                  />
                </label>
                <label>
                  user.email
                  <input
                    type="text"
                    value={email}
                    onChange={(e) => setEmail(e.target.value)}
                    autoComplete="off"
                  />
                </label>
              </div>
              {error && <div className="error">{error}</div>}
              <div className="actions">
                <button
                  type="submit"
                  className="btn primary"
                  disabled={saving || unchanged || (!name.trim() && !email.trim())}
                >
                  {saving ? "Saving…" : "Save"}
                </button>
              </div>
            </form>
          </div>
          <div className="settings-card">
            <h3>GitHub access</h3>
            <div className="kv">
              <span className="k">git</span>
              <span className="v">{settings.gitVersion ?? "not found"}</span>
              <span className="k">Token</span>
              <span className="v">
                {settings.githubTokenSource === "env"
                  ? "GITHUB_TOKEN env var"
                  : settings.githubTokenSource === "stored"
                    ? "token saved in orx"
                    : settings.githubTokenSource === "gh"
                      ? "gh CLI (gh auth token)"
                      : "none found"}
              </span>
            </div>
            {!settings.githubTokenSource && (
              <>
                <p className="settings-note">
                  No GitHub token found — private repo clones and branch pushes will fail. Run{" "}
                  <code>gh auth login</code>, or paste a personal access token:
                </p>
                <GitTokenForm onSaved={setSettings} />
              </>
            )}
            {settings.githubTokenSource === "stored" && (
              <div className="actions">
                <button
                  className="btn"
                  onClick={() => {
                    void removeGitToken().then(setSettings).catch(() => {});
                  }}
                >
                  Remove saved token
                </button>
              </div>
            )}
          </div>
        </>
      )}
    </>
  );
}

// --- page ------------------------------------------------------------------------

const NAV: { id: Tab; label: string; icon: React.ReactNode }[] = [
  { id: "harnesses", label: "Harnesses", icon: <Blocks size={15} /> },
  { id: "compute", label: "Compute", icon: <Cpu size={15} /> },
  { id: "environment", label: "Environment", icon: <SquareTerminal size={15} /> },
  { id: "git", label: "Git", icon: <GitBranch size={15} /> },
];

export function SettingsPage({
  hfSettings,
  hfLoading,
  onHfSettingsUpdated,
  onClose,
  initialTab,
}: {
  hfSettings: HfSettings | null;
  hfLoading: boolean;
  onHfSettingsUpdated: (settings: HfSettings) => void;
  onClose: () => void;
  initialTab?: SettingsTab;
}) {
  const [tab, setTab] = useState<Tab>(initialTab ?? "harnesses");

  return (
    <div className="settings-page">
      <div className="settings-topbar">
        <button className="settings-back" onClick={onClose}>
          <ArrowLeft size={15} /> Back
        </button>
      </div>
      <div className="settings-body">
        <nav className="settings-nav">
          {NAV.map((item) => (
            <button
              key={item.id}
              className={tab === item.id ? "active" : ""}
              onClick={() => setTab(item.id)}
            >
              {item.icon}
              {item.label}
            </button>
          ))}
        </nav>
        <main className="settings-main">
          {tab === "harnesses" && <HarnessesTab />}
          {tab === "compute" && (
            <ComputeTab
              hfSettings={hfSettings}
              hfLoading={hfLoading}
              onHfSettingsUpdated={onHfSettingsUpdated}
            />
          )}
          {tab === "environment" && (
            <>
              <h1>Environment</h1>
              <p className="settings-sub">
                Variables available to runs and the research agent (API keys, tokens).
              </p>
              <EnvVarsSection />
            </>
          )}
          {tab === "git" && <GitTab />}
        </main>
      </div>
    </div>
  );
}
