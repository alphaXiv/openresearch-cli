import {
  Blocks,
  Cpu,
  ExternalLink,
  GitBranch,
  Plus,
  RefreshCw,
  Server,
  SquareTerminal,
  Trash2,
  X,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";
import {
  deleteEnvVar,
  fmtDuration,
  getEnvVars,
  getGitSettings,
  getHarnesses,
  getHfSettings,
  getK8sSettings,
  getModalSettings,
  getSlurmSettings,
  getSshHosts,
  listInstances,
  provisionModal,
  removeGitToken,
  saveGitSettings,
  saveHfToken,
  saveK8sSettings,
  saveSlurmSettings,
  setEnvVar,
  shortId,
  slurmPreflight,
  sshPreflight,
  timeAgo,
  type EnvVar,
  type GitSettings,
  type Harness,
  type HarnessId,
  type HfSettings,
  type HfTokenSource,
  type Instance,
  type K8sSettings,
  type ModalSettings,
  type ModalTokenSource,
  type SlurmPreflight,
  type SlurmSettings,
  type SshHost,
  type SshPreflight,
  modelLabel,
} from "../api";
import { GitTokenForm } from "./GitTokenForm";
import { BackendBadge } from "./BackendLogos";
import { StatusBadge } from "./StatusBadge";

export type SettingsTab = "harnesses" | "compute" | "instances" | "environment" | "git";
type Tab = SettingsTab;

// --- harnesses ---------------------------------------------------------------

function harnessStatus(h: Harness): { cls: string; label: string } {
  // Fully usable only when both the binary is on PATH and it's authenticated.
  if (h.installed && h.authenticated) return { cls: "ok", label: "Connected" };
  // Not installed — the same blocker whether or not there's saved auth: the
  // CLI has to be installed before anything can run. Amber "action needed".
  if (!h.installed) return { cls: "warn", label: "Not installed" };
  // Installed but not signed in.
  return { cls: "warn", label: "Not signed in" };
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
  if (test === undefined) return <span className="muted">never tested</span>;
  if (test === "testing") return <span className="spinner" />;
  const badge = !test.reachable ? (
    <span className="badge err" title={test.error ?? undefined}>unreachable</span>
  ) : !test.gitFound ? (
    <span className="badge err">no git</span>
  ) : (
    <span className="badge ok">ready</span>
  );
  return (
    <>
      {badge}
      <span className="ssh-tested-at">{timeAgo(test.testedAt)}</span>
    </>
  );
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
        [host]: {
          reachable: false,
          gitFound: false,
          error: err instanceof Error ? err.message : String(err),
          testedAt: Date.now(),
        },
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
        <table className="flavor-table ssh-table">
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
                  {/* Session-local result wins; the persisted one covers restarts. */}
                  <HostTestCell test={tests[h.host] ?? h.lastTest} />
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

// --- compute (slurm) --------------------------------------------------------------

/** First failing check wins, like K8sHealthBadge. */
function SlurmTestBadge({ test }: { test: "testing" | SlurmPreflight | null }) {
  if (test === null) return null;
  if (test === "testing") return <span className="spinner" />;
  if (!test.reachable)
    return (
      <span className="badge err" title={test.error ?? undefined}>
        unreachable
      </span>
    );
  if (!test.slurmFound) return <span className="badge err">no slurm CLI</span>;
  if (!test.gitFound) return <span className="badge err">no git</span>;
  return <span className="badge ok">ready</span>;
}

function SlurmSection() {
  const [settings, setSettings] = useState<SlurmSettings | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [host, setHost] = useState("");
  const [partition, setPartition] = useState("");
  const [account, setAccount] = useState("");
  const [timeLimit, setTimeLimit] = useState("");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [test, setTest] = useState<"testing" | SlurmPreflight | null>(null);
  const preflight = test !== null && test !== "testing" ? test : null;

  const apply = (s: SlurmSettings) => {
    setSettings(s);
    setHost(s.host ?? "");
    setPartition(s.partition ?? "");
    setAccount(s.account ?? "");
    setTimeLimit(s.timeLimit ?? "");
  };

  useEffect(() => {
    getSlurmSettings()
      .then(apply)
      .catch((err) => setLoadError(err instanceof Error ? err.message : String(err)));
  }, []);

  const unchanged =
    settings !== null &&
    host === (settings.host ?? "") &&
    partition.trim() === (settings.partition ?? "") &&
    account.trim() === (settings.account ?? "") &&
    timeLimit.trim() === (settings.timeLimit ?? "");

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (saving) return;
    setSaving(true);
    setError(null);
    try {
      apply(
        await saveSlurmSettings({
          host,
          partition: partition.trim(),
          account: account.trim(),
          timeLimit: timeLimit.trim(),
        }),
      );
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSaving(false);
    }
  }

  async function runPreflight(target: string) {
    setTest("testing");
    try {
      setTest(await slurmPreflight(target));
    } catch (err) {
      setTest({
        reachable: false,
        slurmFound: false,
        gitFound: false,
        partitions: [],
        error: err instanceof Error ? err.message : String(err),
      });
    }
  }

  return (
    <>
      <p className="settings-sub">
        <strong>Slurm</strong> — run on your own cluster with{" "}
        <code>--backend slurm [--flavor h100:2]</code>. orx submits via <code>sbatch</code> on
        the login node over ssh (auth is your keys/agent; orx never reads a key) and the job
        runs in your cluster environment. The defaults below apply when a launch doesn&apos;t
        override them.
      </p>
      {loadError ? (
        <div className="settings-card">
          <div className="error">{loadError}</div>
        </div>
      ) : !settings ? (
        <div className="settings-loading">
          <span className="spinner" /> Loading slurm settings…
        </div>
      ) : (
        <div className="settings-card">
          <div className="settings-card-head">
            <h3>Slurm cluster</h3>
            <div className="spacer" style={{ flex: 1 }} />
            <SlurmTestBadge test={test} />
          </div>
          {preflight?.error && <p className="settings-note">{preflight.error}</p>}
          {preflight && preflight.partitions.length > 0 && (
            <p className="settings-note">
              Partitions: <code>{preflight.partitions.join(", ")}</code>
            </p>
          )}
          <form className="form settings-form" onSubmit={submit}>
            <div className="row2">
              <label>
                Login node
                <select
                  value={host}
                  onChange={(e) => {
                    setHost(e.target.value);
                    setTest(null); // a badge earned by cluster A must not vouch for cluster B
                  }}
                >
                  <option value="">not set (pass --host per launch)</option>
                  {/* A saved host that has since left ~/.ssh/config still needs an
                      option, or the select renders blank while holding the value. */}
                  {host && !settings.hosts.some((h) => h.host === host) && (
                    <option value={host}>{host} (not in ~/.ssh/config)</option>
                  )}
                  {settings.hosts.map((h) => (
                    <option key={h.host} value={h.host}>
                      {h.host}
                    </option>
                  ))}
                </select>
              </label>
              <label>
                Partition
                <input
                  className="mono"
                  type="text"
                  list="slurm-partitions"
                  value={partition}
                  onChange={(e) => setPartition(e.target.value)}
                  placeholder="cluster default"
                  autoComplete="off"
                  spellCheck={false}
                />
                <datalist id="slurm-partitions">
                  {preflight?.partitions.map((p) => <option key={p} value={p} />)}
                </datalist>
              </label>
            </div>
            <div className="row2">
              <label>
                Account
                <input
                  className="mono"
                  type="text"
                  value={account}
                  onChange={(e) => setAccount(e.target.value)}
                  placeholder="cluster default"
                  autoComplete="off"
                  spellCheck={false}
                />
              </label>
              <label>
                Time limit
                <input
                  className="mono"
                  type="text"
                  value={timeLimit}
                  onChange={(e) => setTimeLimit(e.target.value)}
                  placeholder="cluster default (e.g. 4h, 30m)"
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
              <button
                type="button"
                className="btn"
                onClick={() => void runPreflight(host)}
                disabled={!host || test === "testing"}
                title={host ? undefined : "Pick a login node first"}
              >
                Test connection
              </button>
            </div>
          </form>
        </div>
      )}
    </>
  );
}

// --- compute -----------------------------------------------------------------

type ComputeSub = "hf" | "modal" | "k8s" | "ssh" | "slurm";

const COMPUTE_SUBS: { id: ComputeSub; label: string }[] = [
  { id: "hf", label: "HF Jobs" },
  { id: "modal", label: "Modal" },
  { id: "k8s", label: "Kubernetes" },
  { id: "ssh", label: "SSH" },
  { id: "slurm", label: "Slurm" },
];

function ComputeTab() {
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
      {sub === "hf" && <HfSection />}
      {sub === "modal" && <ModalSection />}
      {sub === "k8s" && <K8sSection />}
      {sub === "ssh" && <SshSection />}
      {sub === "slurm" && <SlurmSection />}
    </>
  );
}

// --- environment ---------------------------------------------------------------

const SOURCE_LABELS: Record<HfTokenSource, string> = {
  env: "HF_TOKEN env var",
  openresearchEnv: "~/.openresearch/env",
  hfCache: "~/.cache/huggingface/token (hf auth login)",
};

function HfStatusBadge({ settings }: { settings: HfSettings }) {
  if (!settings.configured) return <span className="badge">not configured</span>;
  if (!settings.valid) return <span className="badge err">invalid token</span>;
  return <span className="badge ok">connected</span>;
}

/** Jobs-permission detail only — configured/valid state is HfStatusBadge's job. */
function HfJobsBadge({ settings }: { settings: HfSettings }) {
  if (!settings.configured || !settings.valid) return null;
  if (settings.jobsWrite === true) return <span className="badge ok">jobs: write OK</span>;
  if (settings.jobsWrite === false)
    return <span className="badge err">no job.write permission</span>;
  return <span className="badge">jobs permission unknown</span>;
}

function HfSection() {
  const [settings, setSettings] = useState<HfSettings | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [token, setToken] = useState("");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // A save that lands before the slow mount fetch resolves must win over it.
  const savedRef = useRef(false);

  // Fetched on mount (every visit remounts) so a token set anywhere else —
  // the Environment tab, `hf auth login`, the process env — shows up here.
  useEffect(() => {
    getHfSettings()
      .then((s) => {
        if (!savedRef.current) setSettings(s);
      })
      .catch((err) => {
        if (!savedRef.current) setLoadError(err instanceof Error ? err.message : String(err));
      });
  }, []);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (!token.trim() || saving) return;
    setSaving(true);
    setError(null);
    try {
      const next = await saveHfToken(token.trim());
      savedRef.current = true;
      setSettings(next);
      setLoadError(null);
      setToken("");
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSaving(false);
    }
  }

  return (
    <div className="settings-card">
      <div className="settings-card-head">
        <h3>Hugging Face Jobs</h3>
        <div className="spacer" style={{ flex: 1 }} />
        {settings && <HfStatusBadge settings={settings} />}
      </div>
      <p className="settings-sub">
        Run experiments on your Hugging Face account with{" "}
        <code>--backend hf --flavor &lt;name&gt;</code> (t4-small, a10g-small, a100-large, …).
        Billed to HF per minute.
      </p>
      {loadError ? (
        <div className="error">{loadError}</div>
      ) : !settings ? (
        <div className="settings-loading">
          <span className="spinner" /> Loading status…
        </div>
      ) : (
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
              {(!settings.configured || !settings.valid) && "—"}
            </span>
          </div>
          {settings.source === "env" && (
            <p className="settings-note">
              HF_TOKEN is set in the environment and overrides any token saved here.
            </p>
          )}
          {settings.valid && settings.jobsWrite === null && (
            <p className="settings-note">
              This token is valid but doesn&apos;t report whether it can launch Jobs — OAuth
              tokens from <code>hf auth login</code> never do. Launches may still work; for a
              definitive check, save a write-scoped token from{" "}
              <a href="https://huggingface.co/settings/tokens" target="_blank" rel="noreferrer">
                huggingface.co/settings/tokens
              </a>
              .
            </p>
          )}
        </>
      )}
      <form className="form settings-form" onSubmit={submit}>
        <label>
          {settings?.configured ? "Replace token" : "New token"}
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

// HF user access tokens are `hf_` + alphanumeric. Compute runs resolve the
// token strictly by the name HF_TOKEN, so an hf_… value saved under any other
// key is invisible to them — worth a (non-blocking) warning.
const HF_TOKEN_RE = /^hf_[A-Za-z0-9]{10,}$/;

/** The wrong-key warning shown when an hf_… value is headed somewhere else. */
function HfHintRow() {
  return (
    <tr>
      {/* colSpan tracks the EnvRow/AddVarRow column count */}
      <td colSpan={3}>
        <p className="settings-note">
          This value looks like a Hugging Face token — compute runs only read it from{" "}
          <code>HF_TOKEN</code>. Save it under that key if it&apos;s meant for HF Jobs.
        </p>
      </td>
    </tr>
  );
}

// Keys runs typically need (HF_TOKEN is also read by orx itself), always
// shown as rows alongside custom variables.
const RECOMMENDED_ENV_KEYS = ["HF_TOKEN", "WANDB_API_KEY"];

/** One variable row. Set: masked value + delete. Unset: inline value input. */
function EnvRow({
  name,
  entry,
  onVars,
  onError,
}: {
  name: string;
  entry: EnvVar | undefined;
  onVars: (vars: EnvVar[]) => void;
  onError: (msg: string) => void;
}) {
  const [value, setValue] = useState("");
  const [saving, setSaving] = useState(false);

  // Errors share one card-level slot, so name the row they came from.
  const fail = (err: unknown) =>
    onError(`${name}: ${err instanceof Error ? err.message : String(err)}`);

  async function save() {
    if (!value.trim() || saving) return;
    setSaving(true);
    try {
      onVars(await setEnvVar(name, value.trim()));
      setValue("");
    } catch (err) {
      fail(err);
    } finally {
      setSaving(false);
    }
  }

  async function remove() {
    if (saving) return;
    setSaving(true);
    try {
      onVars(await deleteEnvVar(name));
    } catch (err) {
      fail(err);
    } finally {
      setSaving(false);
    }
  }

  return (
    <>
      <tr>
        <td className="mono">{name}</td>
        <td className="mono muted">
          {entry ? (
            <>
              {entry.maskedValue}
              {entry.inProcessEnv && <span className="badge">overridden by env</span>}
            </>
          ) : (
            <input
              className="mono"
              type="password"
              value={value}
              onChange={(e) => setValue(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  e.preventDefault();
                  void save();
                }
                if (e.key === "Escape" && !saving) setValue("");
              }}
              placeholder="value"
              aria-label={`Value for ${name}`}
              autoComplete="new-password"
              disabled={saving}
            />
          )}
        </td>
        <td>
          {entry ? (
            <button
              className="icon-btn"
              title={`Delete ${name}`}
              aria-label={`Delete ${name}`}
              onClick={() => void remove()}
              disabled={saving}
            >
              <Trash2 size={13} />
            </button>
          ) : (
            value.trim() && (
              <button className="btn sm" onClick={() => void save()} disabled={saving}>
                {saving ? "Saving…" : "Save"}
              </button>
            )
          )}
        </td>
      </tr>
      {!entry && name !== "HF_TOKEN" && HF_TOKEN_RE.test(value.trim()) && <HfHintRow />}
    </>
  );
}

/** The in-table row for a new custom variable (opened by “Add variable”). */
function AddVarRow({
  onVars,
  onError,
  onDone,
}: {
  onVars: (vars: EnvVar[]) => void;
  onError: (msg: string) => void;
  onDone: () => void;
}) {
  const [key, setKey] = useState("");
  const [value, setValue] = useState("");
  const [saving, setSaving] = useState(false);

  async function save() {
    if (!key.trim() || !value.trim() || saving) return;
    setSaving(true);
    try {
      onVars(await setEnvVar(key.trim(), value.trim()));
      onDone();
    } catch (err) {
      onError(`${key.trim()}: ${err instanceof Error ? err.message : String(err)}`);
    } finally {
      setSaving(false);
    }
  }

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "Enter") {
      e.preventDefault();
      void save();
    }
    if (e.key === "Escape" && !saving) onDone();
  };

  return (
    <>
      <tr>
        <td>
          <input
            autoFocus
            className="mono"
            type="text"
            value={key}
            onChange={(e) => setKey(e.target.value)}
            onKeyDown={onKeyDown}
            placeholder="MY_API_KEY"
            aria-label="New variable key"
            autoComplete="off"
            spellCheck={false}
            disabled={saving}
          />
        </td>
        <td>
          <input
            className="mono"
            type="password"
            value={value}
            onChange={(e) => setValue(e.target.value)}
            onKeyDown={onKeyDown}
            placeholder="value"
            aria-label="New variable value"
            autoComplete="new-password"
            disabled={saving}
          />
        </td>
        <td>
          <button
            className="btn sm"
            onClick={() => void save()}
            disabled={saving || !key.trim() || !value.trim()}
          >
            {saving ? "Saving…" : "Save"}
          </button>
          <button
            className="icon-btn"
            title="Cancel"
            aria-label="Cancel new variable"
            onClick={onDone}
            disabled={saving}
          >
            <X size={13} />
          </button>
        </td>
      </tr>
      {key.trim() !== "HF_TOKEN" && HF_TOKEN_RE.test(value.trim()) && <HfHintRow />}
    </>
  );
}

function EnvVarsSection() {
  const [vars, setVars] = useState<EnvVar[] | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    getEnvVars()
      .then(setVars)
      .catch((err) => setLoadError(err instanceof Error ? err.message : String(err)));
  }, []);

  // Every mutation returns the fresh full list; success clears a stale error.
  const applyVars = (v: EnvVar[]) => {
    setVars(v);
    setError(null);
  };

  // Recommended keys first (fixed order), then custom variables in file order.
  const customKeys =
    vars === null ? [] : vars.map((v) => v.key).filter((k) => !RECOMMENDED_ENV_KEYS.includes(k));
  const names = [...RECOMMENDED_ENV_KEYS, ...customKeys];

  return (
    <div className="settings-card">
      <div className="settings-card-head">
        <h3>Environment variables</h3>
        <div className="spacer" style={{ flex: 1 }} />
        <button
          className="btn sm"
          onClick={() => setAdding(true)}
          disabled={adding || vars === null}
        >
          <Plus size={12} /> Add variable
        </button>
      </div>
      <p className="settings-sub">
        Stored in <code>~/.openresearch/env</code> and passed to runs and the research agent.{" "}
        <code>HF_TOKEN</code> and <code>WANDB_API_KEY</code> are always listed since runs
        typically need them. Variables set in orx's own environment win on conflicts.
      </p>
      {loadError ? (
        <div className="error">{loadError}</div>
      ) : vars === null ? (
        <div className="settings-loading">
          <span className="spinner" /> Loading…
        </div>
      ) : (
        <table className="env-table">
          <tbody>
            {names.map((name) => (
              <EnvRow
                key={name}
                name={name}
                entry={vars.find((v) => v.key === name)}
                onVars={applyVars}
                onError={setError}
              />
            ))}
            {adding && (
              // onDone deliberately leaves the error slot alone — cancelling
              // the add row must not wipe another row's failure message.
              <AddVarRow onVars={applyVars} onError={setError} onDone={() => setAdding(false)} />
            )}
          </tbody>
        </table>
      )}
      {error && <div className="error">{error}</div>}
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

// --- instances ---------------------------------------------------------------

const isLive = (status: string) => status === "running" || status === "starting";

/** Runtime: live instances show elapsed-so-far, finished ones total duration.
 *  Both start at submission time, so provisioning/queue time is included —
 *  that's the span the provider bills for. */
function runtimeLabel(inst: Instance): string {
  if (isLive(inst.status)) return fmtDuration(Date.now() - inst.createdAt);
  if (inst.endedAt) return fmtDuration(inst.endedAt - inst.createdAt);
  return "—";
}

/** One section's table: backend (logo + flavor), project, status, started, runtime. */
function InstancesTable({ instances, emptyLabel }: { instances: Instance[]; emptyLabel: string }) {
  if (instances.length === 0) {
    return <p className="instances-empty">{emptyLabel}</p>;
  }
  return (
    <div className="instances-table-wrap">
      <table className="runs-table">
        <thead>
          <tr>
            <th>Backend</th>
            <th>Project</th>
            <th>Status</th>
            <th>Started</th>
            <th>Runtime</th>
          </tr>
        </thead>
        <tbody>
          {instances.map((inst) => {
            // HF jobs carry their dashboard URL; Modal stores only a sandbox id.
            const url = typeof inst.backend?.url === "string" ? inst.backend.url : undefined;
            return (
              <tr key={inst.id}>
                <td>
                  <span className="backend-cell">
                    <BackendBadge backend={inst.backend} />
                    {url && (
                      <a
                        className="icon-btn"
                        href={url}
                        target="_blank"
                        rel="noreferrer"
                        title="Open job page"
                        aria-label="Open job page"
                        onClick={(e) => e.stopPropagation()}
                      >
                        <ExternalLink size={12} />
                      </a>
                    )}
                  </span>
                </td>
                <td>{inst.projectName ?? shortId(inst.projectId)}</td>
                <td>
                  <StatusBadge status={inst.status} />
                </td>
                <td>{timeAgo(inst.createdAt)}</td>
                <td className="mono">{runtimeLabel(inst)}</td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

function InstancesTab() {
  const [instances, setInstances] = useState<Instance[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [refreshing, setRefreshing] = useState(false);

  // Re-render every 30s so live rows' Runtime keeps counting (client-side
  // only — the minute-level display doesn't warrant a refetch).
  const [, setTick] = useState(0);
  useEffect(() => {
    const t = setInterval(() => setTick((n) => n + 1), 30_000);
    return () => clearInterval(t);
  }, []);

  // Point-in-time snapshot: the tab remounts (and so refetches) on every open,
  // and this button refreshes in place while sitting on it — the run.updated
  // SSE stream carries no projectName, so it can't drive this list directly.
  const load = () => {
    setRefreshing(true);
    listInstances()
      .then((rows) => {
        setInstances(rows);
        setError(null);
      })
      .catch((err) => {
        setError(err instanceof Error ? err.message : String(err));
        setInstances((prev) => prev ?? []);
      })
      .finally(() => setRefreshing(false));
  };
  useEffect(() => load(), []);

  const byRecent = (a: Instance, b: Instance) => b.createdAt - a.createdAt;
  const running = instances?.filter((i) => isLive(i.status)).sort(byRecent);
  const past = instances?.filter((i) => !isLive(i.status)).sort(byRecent);

  return (
    <>
      <div className="settings-head-row">
        <h1>Instances</h1>
        <button className="btn sm" onClick={load} disabled={refreshing}>
          <RefreshCw size={12} className={refreshing ? "spin" : ""} /> Refresh
        </button>
      </div>
      <p className="settings-sub">
        Compute spun up across all projects — Modal, Hugging Face, SSH, Kubernetes, Slurm, and OpenResearch.
      </p>
      {error && <div className="error">{error}</div>}
      {!running || !past ? (
        <div className="settings-loading">
          <span className="spinner" /> Loading…
        </div>
      ) : (
        <>
          <h2 className="instances-section-title">
            Running
            {running.length > 0 && <span className="count-badge">{running.length}</span>}
          </h2>
          <InstancesTable instances={running} emptyLabel="Nothing running right now." />
          <h2 className="instances-section-title">Past</h2>
          <InstancesTable instances={past} emptyLabel="No past instances yet." />
        </>
      )}
    </>
  );
}

// --- embedded view -----------------------------------------------------------

/** Rail nav entries, one per settings section (rendered in the agents rail). */
export const SETTINGS_NAV: { id: Tab; label: string; icon: React.ReactNode }[] = [
  { id: "harnesses", label: "Harnesses", icon: <Blocks size={15} /> },
  { id: "compute", label: "Compute", icon: <Cpu size={15} /> },
  { id: "instances", label: "Instances", icon: <Server size={15} /> },
  { id: "environment", label: "Environment", icon: <SquareTerminal size={15} /> },
  { id: "git", label: "Git", icon: <GitBranch size={15} /> },
];

/** One settings section's content, shown in the middle pane in place of chat. */
export function SettingsView({ tab }: { tab: Tab }) {
  return (
    <div className="settings-view">
      {tab === "harnesses" && <HarnessesTab />}
      {tab === "compute" && <ComputeTab />}
      {tab === "environment" && (
        <>
          <h1>Environment</h1>
          <p className="settings-sub">
            Variables available to runs and the research agent (API keys, tokens).
          </p>
          <EnvVarsSection />
        </>
      )}
      {tab === "instances" && <InstancesTab />}
      {tab === "git" && <GitTab />}
    </div>
  );
}
