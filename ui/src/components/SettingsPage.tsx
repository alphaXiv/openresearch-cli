import {
  ArrowLeft,
  ChevronDown,
  Cpu,
  ExternalLink,
  Info,
  Monitor,
  Moon,
  Plus,
  RefreshCw,
  Settings,
  SquareTerminal,
  Sun,
  Trash2,
  X,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";
import {
  deleteEnvVar,
  fmtBytes,
  fmtDuration,
  getComputeSettings,
  getEnvVars,
  getProjectGitStatus,
  getHarnesses,
  getHfSettings,
  getK8sSettings,
  getLocalMachine,
  getModalSettings,
  getOpenResearchSettings,
  getRaySettings,
  getSlurmSettings,
  getSshHosts,
  listInstances,
  setComputeDefault,
  provisionModal,
  initializeProjectGit,
  saveHfToken,
  saveK8sSettings,
  saveRaySettings,
  saveSlurmSettings,
  setEnvVar,
  getDataDir,
  validateDataDir,
  moveDataDir,
  type DataDirSettings,
  type DataDirValidation,
  shortId,
  rayPreflight,
  runDisplayStatus,
  slurmPreflight,
  sshPreflight,
  timeAgo,
  type ComputeSettings,
  type ComputeTargetId,
  type ComputeTargetSummary,
  type EnvVar,
  type Project,
  type ProjectGitStatus,
  type Harness,
  type HarnessId,
  type HfSettings,
  type HfTokenSource,
  type Instance,
  type K8sSettings,
  type LocalMachine,
  type ModalSettings,
  type ModalTokenSource,
  type OpenResearchSettings,
  type RayPreflight,
  type RaySettings,
  type SlurmPreflight,
  type SlurmSettings,
  type SshHost,
  type SshPreflight,
  harnessModelLabel,
} from "../api";
import { onDataDirMove, onHarnessAuth } from "../events";
import { useThemePreference, type ThemePreference } from "../theme";
import { BackendBadge, BackendLogo } from "./BackendLogos";
import { ProgressBar } from "./ProgressBar";
import { StatusBadge } from "./StatusBadge";

export type SettingsTab =
  | "settings"
  | "harnesses"
  | "projects"
  | "compute"
  | "instances"
  | "environment"
  | "git"
  | "storage";
type Tab = SettingsTab;

// --- harnesses ---------------------------------------------------------------

function harnessStatus(h: Harness): { cls: string; label: string } {
  if (h.agentReady) return { cls: "ok", label: "Signed in" };
  // Not installed — the same blocker whether or not there's saved auth: the
  // CLI has to be installed before anything can run. Amber "action needed".
  if (!h.installed) return { cls: "warn", label: "Not installed" };
  if (h.authState === "unknown") return { cls: "warn", label: "Unable to verify" };
  if (h.authState === "unsupported") return { cls: "warn", label: "Update required" };
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

  const load = (refresh: boolean, retryRejected = false) => {
    setRefreshing(true);
    getHarnesses(refresh, retryRejected)
      .then(setHarnesses)
      .catch(() => {})
      .finally(() => setRefreshing(false));
  };
  useEffect(() => load(false), []);
  useEffect(() => onHarnessAuth(() => load(true)), []);

  const h = harnesses?.find((x) => x.id === active);

  return (
    <>
      <h2>Harnesses</h2>
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
            <button className="btn sm" onClick={() => load(true, true)} disabled={refreshing}>
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
                    .map((m) => harnessModelLabel(m))
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
  if (!s.configured) return <span className="badge">Not configured</span>;
  const p = s.preflight;
  if (!p.kubectlFound) return <span className="badge err">kubectl not found</span>;
  if (!p.reachable) return <span className="badge err">Cluster unreachable</span>;
  if (!p.canCreateJobs) return <span className="badge err">No job-create permission</span>;
  return <span className="badge ok">Connected</span>;
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
        Run on your own cluster with <code>--backend k8s</code>. The run&apos;s resources
        (image, GPUs, topology) come from a manifest committed on the experiment branch
        (default <code>.orx/k8s.yaml</code>); only the cluster context and namespace live
        here. Auth comes from your kubeconfig.
      </p>
      {loadError ? (
        <div className="error">{loadError}</div>
      ) : !settings ? (
        <div className="settings-loading">
          <span className="spinner" /> Checking kubectl…
        </div>
      ) : (
        <>
          <div className="kv">
            <span className="k">Cluster</span>
            <span className="v">
              <K8sHealthBadge s={settings} />
            </span>
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
  if (s.ready) return <span className="badge ok">Connected</span>;
  if (!s.tokenConfigured && !s.modalImportable) return <span className="badge">Not set up</span>;
  if (!s.modalImportable)
    return <span className="badge err">{s.envProvisioned ? "Env broken" : "Env not built"}</span>;
  if (!s.tokenConfigured) return <span className="badge err">No token</span>;
  return <span className="badge">Unknown</span>;
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
    <>
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
            <span className="k">Status</span>
            <span className="v">
              <ModalBadge s={s} />
            </span>
            <span className="k">Environment</span>
            <span className="v">
              {s.modalImportable
                ? "Ready"
                : s.envProvisioned
                  ? "Provisioned (modal import failing)"
                  : "Not built yet"}
            </span>
            <span className="k">Token</span>
            <span className="v">
              {s.tokenSource ? MODAL_TOKEN_LABELS[s.tokenSource] : "Not configured"}
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
    </>
  );
}

// --- compute (ssh) ---------------------------------------------------------------

type HostTest = "testing" | SshPreflight;

function HostTestCell({ test }: { test: HostTest | undefined }) {
  if (test === undefined) return <span className="muted">never tested</span>;
  if (test === "testing") return <span className="spinner" />;
  const badge = !test.reachable ? (
    <span className="badge err" title={test.error ?? undefined}>Unreachable</span>
  ) : !test.toolsFound ? (
    <span className="badge err">No bash/tar</span>
  ) : (
    <span className="badge ok">Ready</span>
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
          toolsFound: false,
          error: err instanceof Error ? err.message : String(err),
          testedAt: Date.now(),
        },
      }));
    }
  }

  return (
    <>
      <p className="settings-sub">
        Run experiments directly on your own boxes with{" "}
        <code>--backend ssh --host &lt;alias&gt;</code>. Hosts come from{" "}
        <code>~/.ssh/config</code>; auth uses your keys/agent (orx never reads a key). The host
        just needs <code>bash</code> and <code>tar</code>.
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
    </>
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
        Unreachable
      </span>
    );
  if (!test.slurmFound) return <span className="badge err">No Slurm CLI</span>;
  if (!test.toolsFound) return <span className="badge err">No bash/tar</span>;
  return <span className="badge ok">Ready</span>;
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
        toolsFound: false,
        partitions: [],
        error: err instanceof Error ? err.message : String(err),
      });
    }
  }

  return (
    <>
      <p className="settings-sub">
        Run on your own cluster with <code>--backend slurm [--flavor h100:2]</code>. orx
        submits via <code>sbatch</code> on the login node over ssh (auth is your keys/agent;
        orx never reads a key) and the job runs in your cluster environment. The defaults
        below apply when a launch doesn&apos;t override them.
      </p>
      {loadError ? (
        <div className="error">{loadError}</div>
      ) : !settings ? (
        <div className="settings-loading">
          <span className="spinner" /> Loading slurm settings…
        </div>
      ) : (
        <>
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
              <SlurmTestBadge test={test} />
            </div>
          </form>
        </>
      )}
    </>
  );
}

function RaySection() {
  const [settings, setSettings] = useState<RaySettings | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [address, setAddress] = useState("");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [test, setTest] = useState<"testing" | RayPreflight | null>(null);
  const preflight = test !== null && test !== "testing" ? test : null;

  const apply = (s: RaySettings) => {
    setSettings(s);
    setAddress(s.address ?? "");
  };

  useEffect(() => {
    getRaySettings()
      .then(apply)
      .catch((err) => setLoadError(err instanceof Error ? err.message : String(err)));
  }, []);

  const unchanged = settings !== null && address === (settings.address ?? "");

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (saving) return;
    setSaving(true);
    setError(null);
    try {
      apply(await saveRaySettings({ address }));
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSaving(false);
    }
  }

  async function runPreflight() {
    setTest("testing");
    try {
      setTest(await rayPreflight(address.trim() || undefined));
    } catch (err) {
      setTest({
        reachable: false,
        address: address.trim() || "(unknown)",
        rayVersion: null,
        error: err instanceof Error ? err.message : String(err),
      });
    }
  }

  return (
    <>
      <p className="settings-sub">
        Run on a Ray cluster with <code>--backend ray [--flavor gpu:1]</code>. orx
        submits via the Ray Jobs API (Dashboard URL). Address resolution: this
        setting, then <code>ASTROAI_RAY_JOBS_ADDRESS</code> /{" "}
        <code>RAY_DASHBOARD_URL</code>, then <code>http://127.0.0.1:8265</code>.
        Optional flavor maps to entrypoint CPUs/GPUs/memory (e.g.{" "}
        <code>cpu:2</code>, <code>gpu:1,mem:8GiB</code>).
      </p>
      {loadError ? (
        <div className="error">{loadError}</div>
      ) : !settings ? (
        <div className="settings-loading">
          <span className="spinner" /> Loading Ray settings…
        </div>
      ) : (
        <>
          <p className="settings-note">
            Effective: <code>{settings.resolvedAddress}</code> ({settings.source})
          </p>
          {preflight?.error && <p className="settings-note">{preflight.error}</p>}
          {preflight?.reachable && preflight.rayVersion && (
            <p className="settings-note">
              Ray version: <code>{preflight.rayVersion}</code>
            </p>
          )}
          <form className="form settings-form" onSubmit={submit}>
            <label>
              Jobs / Dashboard URL
              <input
                className="mono"
                type="text"
                value={address}
                onChange={(e) => {
                  setAddress(e.target.value);
                  setTest(null);
                }}
                placeholder="http://127.0.0.1:8265"
                autoComplete="off"
                spellCheck={false}
              />
            </label>
            {error && <div className="error">{error}</div>}
            <div className="actions">
              <button type="submit" className="btn primary" disabled={saving || unchanged}>
                {saving ? "Saving…" : "Save"}
              </button>
              <button
                type="button"
                className="btn"
                onClick={() => void runPreflight()}
                disabled={test === "testing"}
              >
                Test connection
              </button>
              <RayTestBadge test={test} />
            </div>
          </form>
        </>
      )}
    </>
  );
}

function RayTestBadge({ test }: { test: "testing" | RayPreflight | null }) {
  if (test === null) return null;
  if (test === "testing") return <span className="badge">Testing…</span>;
  if (test.reachable) return <span className="badge ok">Reachable</span>;
  return <span className="badge warn">Unreachable</span>;
}

// --- compute (local) --------------------------------------------------------------

function LocalSection() {
  const [hw, setHw] = useState<LocalMachine | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);

  useEffect(() => {
    getLocalMachine()
      .then(setHw)
      .catch((err) => setLoadError(err instanceof Error ? err.message : String(err)));
  }, []);

  return (
    <>
      <p className="settings-sub">
        Run experiments as detached, supervised processes on the machine running orx with{" "}
        <code>--backend local</code> — handy when you&apos;re already on a GPU box and using
        this dashboard over port forwarding. Runs share CPU/RAM/GPU with the dashboard
        itself, so prefer a remote backend for anything heavy.
      </p>
      {loadError ? (
        <div className="error">{loadError}</div>
      ) : !hw ? (
        <div className="settings-loading">
          <span className="spinner" /> Detecting hardware…
        </div>
      ) : (
        <div className="kv">
          <span className="k">Hostname</span>
          <span className="v mono">{hw.hostname}</span>
          <span className="k">System</span>
          <span className="v">
            {hw.os}/{hw.arch}
            {hw.chip ? ` — ${hw.chip}` : ""}
          </span>
          <span className="k">CPU</span>
          <span className="v">{hw.cpuCount > 0 ? `${hw.cpuCount} cores` : "—"}</span>
          <span className="k">RAM</span>
          <span className="v">{hw.memBytes !== null ? fmtBytes(hw.memBytes) : "—"}</span>
          <span className="k">GPUs</span>
          <span className="v">
            {hw.gpus.length === 0
              ? "none detected (nvidia-smi)"
              : hw.gpus
                  .map(
                    (g) =>
                      `${g.name}${g.memMib !== null ? ` — ${fmtBytes(g.memMib * 1024 * 1024)}` : ""}`,
                  )
                  .join(", ")}
          </span>
        </div>
      )}
    </>
  );
}

// --- compute (openresearch) ---------------------------------------------------------

function OpenResearchSection() {
  const [s, setS] = useState<OpenResearchSettings | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);

  useEffect(() => {
    getOpenResearchSettings()
      .then(setS)
      .catch((err) => setLoadError(err instanceof Error ? err.message : String(err)));
  }, []);

  return (
    <>
      <p className="settings-sub">
        Run on an ephemeral OpenResearch box billed to your org with{" "}
        <code>--backend openresearch --flavor &lt;shape&gt;</code> (h100_sxm, cpu5c, …; browse
        with <code>orx compute</code>). The box is provisioned for the run and deleted when it
        ends. Needs <code>orx login</code> and a registered SSH key.
      </p>
      {loadError ? (
        <div className="error">{loadError}</div>
      ) : !s ? (
        <div className="settings-loading">
          <span className="spinner" /> Checking credentials…
        </div>
      ) : !s.loggedIn ? (
        <p className="settings-note">
          Not signed in. Run <code>orx login</code> in a terminal to connect your OpenResearch
          account.
        </p>
      ) : (
        <>
          <div className="kv">
            <span className="k">Status</span>
            <span className="v">
              <span className="badge ok">Signed in</span>
            </span>
            <span className="k">Orgs</span>
            <span className="v">{s.orgs.length > 0 ? s.orgs.join(", ") : "—"}</span>
            <span className="k">SSH key</span>
            <span className="v">
              {s.sshKeyStatus === "matched" ? (
                <span className="badge ok">On this computer</span>
              ) : s.sshKeyStatus === "no_local_match" ? (
                <span className="badge warn">Not on this computer</span>
              ) : s.sshKeyStatus === "none_registered" ? (
                <span className="badge err">None registered</span>
              ) : (
                <span className="badge">Unknown</span>
              )}
            </span>
          </div>
          {s.sshKeyStatus === "none_registered" &&
            (s.sshKeyPath ? (
              <p className="settings-note">
                Add one with <code>orx ssh-key add {s.sshKeyPath}</code>.
              </p>
            ) : (
              <p className="settings-note">
                No key on this computer yet — create one with{" "}
                <code>ssh-keygen -t ed25519</code>, then add it with{" "}
                <code>orx ssh-key add</code>.
              </p>
            ))}
          {s.sshKeyStatus === "no_local_match" &&
            (s.sshKeyPath ? (
              <p className="settings-note">
                Register this computer with <code>orx ssh-key add {s.sshKeyPath}</code>,
                or load a registered key with <code>ssh-add</code>.
              </p>
            ) : (
              <p className="settings-note">
                No key on this computer to register — load a registered key with{" "}
                <code>ssh-add</code>, or create one with{" "}
                <code>ssh-keygen -t ed25519</code>.
              </p>
            ))}
          {s.error && <p className="settings-note">{s.error}</p>}
        </>
      )}
    </>
  );
}

// --- compute -----------------------------------------------------------------

const TARGET_LABELS: Record<ComputeTargetId, string> = {
  local: "This machine",
  hf: "HF Jobs",
  modal: "Modal",
  k8s: "Kubernetes",
  ssh: "SSH",
  slurm: "Slurm",
  ray: "Ray",
  openresearch: "OpenResearch",
};

/** Kind strings from the runs table — reuses the instances-table logos. */
const TARGET_KIND: Record<ComputeTargetId, string> = {
  local: "local_job",
  hf: "hf_job",
  modal: "modal_job",
  k8s: "k8s_job",
  ssh: "ssh_job",
  slurm: "slurm_job",
  ray: "ray_job",
  openresearch: "openresearch_job",
};

/** Backends whose launches take --flavor; mirrors the server's validation. */
const FLAVORED_TARGETS: ComputeTargetId[] = ["hf", "modal", "slurm", "ray", "openresearch"];
/** Of those, the ones where a launch *requires* a flavor. */
const FLAVOR_REQUIRED: ComputeTargetId[] = ["hf", "modal", "openresearch"];

const FLAVOR_SUGGESTIONS: Partial<Record<ComputeTargetId, string[]>> = {
  hf: ["cpu-basic", "t4-small", "a10g-small", "a10g-large", "a100-large", "h100", "h200"],
  modal: ["cpu", "t4", "l4", "a10g", "a100", "a100-80gb", "l40s", "h100", "h100:2"],
  slurm: ["gpu", "h100:1", "h100:2", "a100:4"],
  ray: ["cpu", "cpu:2", "gpu", "gpu:1", "gpu:1,cpu:4", "gpu:1,mem:8GiB"],
  openresearch: ["h100_sxm", "h100_sxm:2", "cpu5c", "cpu5g", "cpu5m"],
};

function TargetStatusBadge({ t, isDefault }: { t: ComputeTargetSummary; isDefault: boolean }) {
  if (t.id === "local") return <span className="badge ok">Ready</span>;
  // Don't claim either answer when the check couldn't run.
  if (t.unverified) return <span className="badge">Unknown</span>;
  if (!t.configured && isDefault) return <span className="badge warn">Not configured</span>;
  if (!t.configured) return <span className="badge">Not set up</span>;
  return <span className="badge ok">Configured</span>;
}

/** The default row's inline flavor editor (flavored backends only). */
function DefaultFlavorEditor({
  target,
  flavor,
  projectId,
  onSaved,
}: {
  target: ComputeTargetId;
  flavor: string | null;
  projectId?: string;
  onSaved: (s: ComputeSettings) => void;
}) {
  const [value, setValue] = useState(flavor ?? "");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // Reflect an outside change (e.g. default moved to another backend and back).
  useEffect(() => setValue(flavor ?? ""), [flavor]);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (saving) return;
    setSaving(true);
    setError(null);
    try {
      onSaved(await setComputeDefault({ backend: target, flavor: value.trim() || null, projectId }));
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSaving(false);
    }
  }

  const unchanged = value.trim() === (flavor ?? "");
  return (
    <form className="form settings-form compute-flavor-form" onSubmit={submit}>
      <label>
        Default flavor
        <input
          className="mono"
          type="text"
          list={`flavors-${target}`}
          value={value}
          onChange={(e) => setValue(e.target.value)}
          placeholder={
            FLAVOR_REQUIRED.includes(target)
              ? `e.g. ${FLAVOR_SUGGESTIONS[target]?.[1] ?? ""}`
              : "none (CPU-only)"
          }
          autoComplete="off"
          spellCheck={false}
        />
        <datalist id={`flavors-${target}`}>
          {(FLAVOR_SUGGESTIONS[target] ?? []).map((f) => (
            <option key={f} value={f} />
          ))}
        </datalist>
      </label>
      {error && <div className="error">{error}</div>}
      <div className="actions">
        <button type="submit" className="btn sm" disabled={saving || unchanged}>
          {saving ? "Saving…" : "Save flavor"}
        </button>
        {FLAVOR_REQUIRED.includes(target) && !flavor && (
          <span className="muted compute-flavor-hint">
            This backend requires a flavor — without a default one, each launch must pass{" "}
            <code>--flavor</code>.
          </span>
        )}
      </div>
    </form>
  );
}

function TargetRow({
  target,
  isDefault,
  isFallbackDefault,
  defaultFlavor,
  open,
  onToggle,
  onSettings,
  onError,
  projectId,
}: {
  target: ComputeTargetSummary;
  isDefault: boolean;
  isFallbackDefault: boolean;
  defaultFlavor: string | null;
  open: boolean;
  onToggle: () => void;
  onSettings: (s: ComputeSettings) => void;
  onError: (msg: string) => void;
  projectId?: string;
}) {
  // Mounted on first expand, kept mounted (hidden) after — each section's own
  // mount-time fetch is the lazy detail load, and re-expanding doesn't refetch.
  const [visited, setVisited] = useState(false);
  const [settingDefault, setSettingDefault] = useState(false);
  if (open && !visited) setVisited(true);

  async function setDefault(backend: ComputeTargetId | null) {
    if (settingDefault) return;
    setSettingDefault(true);
    try {
      onSettings(await setComputeDefault({ backend, projectId }));
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    } finally {
      setSettingDefault(false);
    }
  }

  return (
    <div className={`compute-row${open ? " open" : ""}${target.enabled ? "" : " disabled"}`}>
      {/* The head is a plain clickable div, NOT role="button": it holds real
          buttons (Make default, the chevron), and interactive elements must
          not nest. The chevron is the keyboard-reachable expand control. */}
      <div className="compute-row-head" onClick={target.enabled ? onToggle : undefined}>
        <span className="compute-row-logo">
          <BackendLogo kind={TARGET_KIND[target.id]} size={18} />
        </span>
        <span className="compute-row-name">{TARGET_LABELS[target.id]}</span>
        <span className="compute-row-summary">{target.summary}</span>
        <TargetStatusBadge t={target} isDefault={isDefault} />
        {isDefault ? (
          <span className="badge compute-default-pill">{isFallbackDefault ? "Local fallback" : "Default"}</span>
        ) : (
          <button
            type="button"
            className="btn sm compute-make-default"
            onClick={(e) => {
              e.stopPropagation(); // the header click is expand/collapse
              void setDefault(target.id);
            }}
            disabled={settingDefault || !target.enabled}
          >
            Make default
          </button>
        )}
        <button
          type="button"
          className="compute-chevron-btn"
          aria-expanded={open}
          aria-label={`${open ? "Collapse" : "Expand"} ${TARGET_LABELS[target.id]}`}
          disabled={!target.enabled}
          onClick={(e) => {
            e.stopPropagation();
            if (target.enabled) onToggle();
          }}
        >
          <ChevronDown size={16} className="compute-chevron" />
        </button>
      </div>
      {visited && target.enabled && (
        <div className="compute-row-body" hidden={!open}>
          {isDefault && !isFallbackDefault && (
            <p className="settings-note compute-default-note">
              The agent launches runs here unless you tell it otherwise, and so does{" "}
              <code>orx exp run</code> with no <code>--backend</code> flag.{" "}
              <button
                type="button"
                className="btn sm"
                onClick={() => void setDefault(null)}
                disabled={settingDefault}
              >
                Clear default
              </button>
            </p>
          )}
          {isDefault && !target.configured && (
            <p className="settings-note">
              This target is the default but isn&apos;t configured — launches will fail until
              it&apos;s set up below.
            </p>
          )}
          {isDefault && FLAVORED_TARGETS.includes(target.id) && (
            <DefaultFlavorEditor target={target.id} flavor={defaultFlavor} projectId={projectId} onSaved={onSettings} />
          )}
          {target.id === "local" && <LocalSection />}
          {target.id === "hf" && <HfSection />}
          {target.id === "modal" && <ModalSection />}
          {target.id === "k8s" && <K8sSection />}
          {target.id === "ssh" && <SshSection />}
          {target.id === "slurm" && <SlurmSection />}
          {target.id === "ray" && <RaySection />}
          {target.id === "openresearch" && <OpenResearchSection />}
        </div>
      )}
    </div>
  );
}

function ComputeTab({
  project,
  onViewHistory,
}: {
  project: Project | null;
  onViewHistory: () => void;
}) {
  const [settings, setSettings] = useState<ComputeSettings | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [expanded, setExpanded] = useState<ComputeTargetId | null>(null);
  const [error, setError] = useState<string | null>(null);
  // Monotonic guard: a POST response applied via `apply` must not be
  // overwritten by a slower background GET that was already in flight.
  const seqRef = useRef(0);

  useEffect(() => {
    seqRef.current++;
    setSettings(null);
    setExpanded(null);
    setLoadError(null);
    setError(null);
  }, [project?.id]);

  // Refetched whenever a row expands/collapses (not just on mount): a form
  // saved inside a row (k8s context, HF token, …) changes the collapsed
  // summaries, and the toggle is the natural moment to catch up. Cheap by
  // contract — the endpoint only does fs/env probes.
  useEffect(() => {
    const seq = ++seqRef.current;
    getComputeSettings(project?.id)
      .then((s) => {
        if (seq !== seqRef.current) return;
        setSettings(s);
        setLoadError(null);
      })
      .catch((err) => {
        if (seq !== seqRef.current) return;
        // Only the very first load may brick the tab; a failed background
        // refresh of already-rendered rows goes to the transient banner.
        const msg = err instanceof Error ? err.message : String(err);
        setSettings((cur) => {
          if (cur === null) setLoadError(msg);
          else setError(msg);
          return cur;
        });
      });
  }, [expanded, project?.id]);

  const apply = (s: ComputeSettings) => {
    seqRef.current++; // supersede any in-flight background GET
    setSettings(s);
    setError(null);
  };

  // Server order is canonical (local first, then external backends).
  const targets = settings ? settings.targets : null;
  const fallbackDefault =
    settings?.defaultBackend === "local" &&
    settings.configuredDefaultBackend !== null &&
    settings.configuredDefaultBackend !== undefined &&
    settings.configuredDefaultBackend !== "local";
  const renderTarget = (target: ComputeTargetSummary) => (
    <TargetRow
      key={`${project?.id ?? "none"}:${target.id}`}
      target={target}
      isDefault={settings?.defaultBackend === target.id}
      isFallbackDefault={Boolean(fallbackDefault && target.id === "local")}
      defaultFlavor={settings?.defaultFlavor ?? null}
      open={target.enabled && expanded === target.id}
      onToggle={() => setExpanded((current) => (current === target.id ? null : target.id))}
      onSettings={apply}
      onError={setError}
      projectId={project?.id}
    />
  );

  return (
    <>
      <h1>Compute</h1>
      <p className="settings-sub">
        Where <code>orx exp run</code> executes. Pick a default target; the agent uses it when
        a launch doesn&apos;t name a backend (<code>--backend &lt;name&gt;</code> always wins).
      </p>
      <ComputeActivity onViewHistory={onViewHistory} />
      <h2 className="compute-section-title">Targets</h2>
      {loadError ? (
        <div className="error">{loadError}</div>
      ) : !targets ? (
        <div className="settings-loading">
          <span className="spinner" /> Checking compute targets…
        </div>
      ) : (
        <>
          {error && <div className="error">{error}</div>}
          <div className="compute-list">
            {targets.filter((target) => target.id === "local").map(renderTarget)}
            {targets.filter((target) => target.id !== "local").map(renderTarget)}
          </div>
          <p className="compute-footnote">
            <Info size={14} aria-hidden="true" />
            <span>
              The default target and flavor are included in the research agent&apos;s
              instructions — it launches runs there unless you name another backend. No other
              compute settings are shared with it.
            </span>
          </p>
        </>
      )}
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
  if (!settings.configured) return <span className="badge">Not configured</span>;
  if (!settings.valid) return <span className="badge err">Invalid token</span>;
  return <span className="badge ok">Connected</span>;
}

/** Jobs-permission detail only — configured/valid state is HfStatusBadge's job. */
function HfJobsBadge({ settings }: { settings: HfSettings }) {
  if (!settings.configured || !settings.valid) return null;
  if (settings.jobsWrite === true) return <span className="badge ok">Jobs: write OK</span>;
  if (settings.jobsWrite === false)
    return <span className="badge err">No job.write permission</span>;
  return <span className="badge">Jobs permission unknown</span>;
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
    <>
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
            <span className="k">Status</span>
            <span className="v">
              <HfStatusBadge settings={settings} />
            </span>
            <span className="k">Account</span>
            <span className="v">{settings.username ?? "—"}</span>
            <span className="k">Token</span>
            <span className="v">{settings.maskedToken ?? "—"}</span>
            <span className="k">Source</span>
            <span className="v">
              {settings.source ? SOURCE_LABELS[settings.source] : "Not configured"}
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
    </>
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
              {entry.inProcessEnv && <span className="badge">Overridden by env</span>}
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

// --- appearance ----------------------------------------------------------------

const THEME_OPTIONS: {
  value: ThemePreference;
  label: string;
  icon: typeof Monitor;
}[] = [
  { value: "system", label: "System", icon: Monitor },
  { value: "light", label: "Light", icon: Sun },
  { value: "dark", label: "Dark", icon: Moon },
];

function AppearanceTab() {
  const [preference, setPreference] = useThemePreference();

  // Arrow keys move selection relative to the focused radio, with focus
  // following the new choice (WAI-ARIA radio pattern).
  const onKeyDown = (e: React.KeyboardEvent) => {
    const dir =
      e.key === "ArrowRight" || e.key === "ArrowDown"
        ? 1
        : e.key === "ArrowLeft" || e.key === "ArrowUp"
          ? -1
          : 0;
    if (!dir) return;
    e.preventDefault();
    const radios = [
      ...e.currentTarget.querySelectorAll<HTMLButtonElement>('[role="radio"]'),
    ];
    const from = radios.findIndex((r) => r === document.activeElement);
    const anchor =
      from === -1 ? THEME_OPTIONS.findIndex((o) => o.value === preference) : from;
    const next = (anchor + dir + THEME_OPTIONS.length) % THEME_OPTIONS.length;
    setPreference(THEME_OPTIONS[next].value);
    radios[next]?.focus();
  };

  return (
    <>
      <h2>Appearance</h2>
      <p className="settings-sub">How the interface looks on this device.</p>
      <div className="settings-card">
        <div className="project-default-row">
          <div>
            <div className="project-default-title">Theme</div>
            <p>System follows your operating system's light or dark setting.</p>
          </div>
          <div
            className="theme-segmented"
            role="radiogroup"
            aria-label="Theme"
            onKeyDown={onKeyDown}
          >
            {THEME_OPTIONS.map(({ value, label, icon: Icon }) => (
              <button
                key={value}
                type="button"
                role="radio"
                aria-checked={preference === value}
                tabIndex={preference === value ? 0 : -1}
                className={`theme-segment ${preference === value ? "on" : ""}`}
                onClick={() => setPreference(value)}
              >
                <Icon size={14} />
                {label}
              </button>
            ))}
          </div>
        </div>
      </div>
    </>
  );
}

// --- git -----------------------------------------------------------------------

function GitTab({ project }: { project: Project | null }) {
  const [status, setStatus] = useState<ProjectGitStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const seqRef = useRef(0);

  const load = () => {
    const request = ++seqRef.current;
    setStatus(null);
    setError(null);
    if (!project) return;
    void getProjectGitStatus(project.id)
      .then((projectStatus) => {
        if (request !== seqRef.current) return;
        setStatus(projectStatus);
      })
      .catch((err) => {
        if (request === seqRef.current) {
          setError(err instanceof Error ? err.message : String(err));
        }
      });
  };
  useEffect(load, [project?.id]);

  return (
    <>
      <h1>Repository</h1>
      <p className="settings-sub">
        Local Git settings for <strong>{project?.name ?? "the current project"}</strong>.
        Runs use immutable snapshots of committed revisions.
      </p>
      {!project ? (
        <div className="settings-card"><p className="settings-note">Open a project to inspect its repository.</p></div>
      ) : error && !status ? (
        <div className="error">{error}</div>
      ) : !status ? (
        <div className="settings-loading">
          <span className="spinner" /> Loading…
        </div>
      ) : (
        <>
          <div className="settings-card git-settings-card">
            <h3>Local repository</h3>
            <div className="kv">
              <span className="k">Path</span><span className="v mono">{status.path}</span>
              <span className="k">Git</span><span className="v">{status.gitVersion ?? "not found"}</span>
              <span className="k">State</span><span className="v">{status.initialized ? `${status.currentBranch ?? "detached"} · ${status.clean ? "clean" : "has changes"}` : "not initialized"}</span>
              <span className="k">Baseline</span><span className="v mono">{status.baselineBranch}</span>
              <span className="k">Remotes</span><span className="v">{status.remotes.length ? status.remotes.map((remote) => `${remote.name}: ${remote.url}`).join(" · ") : "none"}</span>
            </div>
            {!status.initialized && <div className="git-card-actions"><button className="btn primary" onClick={() => void initializeProjectGit(project.id).then(setStatus).catch((err) => setError(String(err)))}>Initialize Git</button></div>}
          </div>
          {error && <div className="error">{error}</div>}
        </>
      )}
    </>
  );
}

// --- storage (data directory) ------------------------------------------------

const DATA_DIR_SOURCE_LABEL: Record<DataDirSettings["source"], string> = {
  env: "ORX_DATA_DIR environment variable",
  config: "your saved setting",
  xdg: "XDG_DATA_HOME",
  default: "default location",
};

type MoveState =
  | { kind: "idle" }
  | { kind: "moving"; phase: string; copied: number; total: number }
  | { kind: "done"; oldPathLeft?: string }
  | { kind: "error"; message: string };

function StorageTab() {
  const [settings, setSettings] = useState<DataDirSettings | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [path, setPath] = useState("");
  const [checking, setChecking] = useState(false);
  const [validation, setValidation] = useState<DataDirValidation | null>(null);
  const [move, setMove] = useState<MoveState>({ kind: "idle" });
  const [error, setError] = useState<string | null>(null);

  const load = () =>
    getDataDir()
      .then((s) => {
        setSettings(s);
        // Seed the input to the current path only when empty — preserves an
        // in-progress edit, and (after a move clears it) re-seeds to the new path.
        setPath((p) => (p ? p : s.current));
      })
      .catch((err) => setLoadError(err instanceof Error ? err.message : String(err)));

  useEffect(() => {
    void load();
  }, []);

  // Subscribe to move progress streamed over the shared SSE.
  useEffect(() => {
    return onDataDirMove((ev) => {
      if (ev.type === "progress") {
        // The "preparing" tick reports total 0 (sized after the checkpoint);
        // keep the last known non-zero total so the bar doesn't flicker to 0.
        setMove((m) => {
          const prevTotal = m.kind === "moving" ? m.total : 0;
          return {
            kind: "moving",
            phase: ev.phase,
            copied: ev.copiedBytes,
            total: ev.totalBytes || prevTotal,
          };
        });
      } else if (ev.type === "done") {
        setMove({ kind: "done", oldPathLeft: ev.oldPathLeft });
        setValidation(null);
        // Clear so load()'s empty-guard re-seeds the input to the new path.
        setPath("");
        void load();
      } else if (ev.type === "error") {
        setMove({ kind: "error", message: ev.error });
      }
    });
  }, []);

  const envForced = settings?.source === "env";
  const trimmed = path.trim();
  const unchanged = settings !== null && trimmed === settings.current;

  async function check() {
    if (checking || !trimmed) return;
    setChecking(true);
    setError(null);
    setValidation(null);
    try {
      setValidation(await validateDataDir(trimmed));
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setChecking(false);
    }
  }

  async function startMove(e: React.FormEvent) {
    e.preventDefault();
    if (move.kind === "moving" || !trimmed || unchanged) return;
    setError(null);
    // Confirm — this relocates all projects' data. Same-disk moves are atomic;
    // cross-disk moves copy and leave the old folder for you to remove.
    if (
      !window.confirm(
        `Move all orx data to:\n${trimmed}\n\nThe store is copied to the new location and ` +
          `activated there. Active runs or chats will block the move.`,
      )
    )
      return;
    setMove({ kind: "moving", phase: "preparing", copied: 0, total: validation?.treeBytes ?? 0 });
    try {
      await moveDataDir(trimmed);
      // 202 accepted — progress/done arrive over SSE. Nothing else to do here.
    } catch (err) {
      // 409 in-flight guard or a validation error surfaces here.
      setMove({ kind: "idle" });
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  return (
    <>
      <h2>Storage</h2>
      <p className="settings-sub">
        Where orx keeps everything on this machine — the local database, run logs, artifacts, and
        chat attachments for <strong>all</strong> projects. Moving it copies the whole store to the
        new location and activates it there.
      </p>
      {loadError ? (
        <div className="settings-card">
          <div className="error">{loadError}</div>
        </div>
      ) : !settings ? (
        <div className="settings-loading">
          <span className="spinner" /> Loading…
        </div>
      ) : (
        <div className="settings-card">
          <div className="settings-card-head">
            <h3>Data directory</h3>
            <div className="spacer" style={{ flex: 1 }} />
            <span className="badge">{settings.isDefault ? "Default" : "Custom"}</span>
          </div>
          <div className="kv">
            <span className="k">Current</span>
            <span className="v mono">{settings.current}</span>
            <span className="k">Source</span>
            <span className="v">{DATA_DIR_SOURCE_LABEL[settings.source]}</span>
            {!settings.isDefault && (
              <>
                <span className="k">Default</span>
                <span className="v mono">{settings.defaultPath}</span>
              </>
            )}
          </div>

          {envForced ? (
            <p className="settings-note">
              The data directory is pinned by the <code>ORX_DATA_DIR</code> environment variable,
              which overrides this setting. Unset it to choose a location here.
            </p>
          ) : (
            <form className="form settings-form" onSubmit={startMove}>
              <label>
                New location
                <input
                  className="mono"
                  type="text"
                  value={path}
                  onChange={(e) => {
                    setPath(e.target.value);
                    setValidation(null);
                  }}
                  placeholder="/absolute/path/to/openresearch"
                  autoComplete="off"
                  spellCheck={false}
                  disabled={move.kind === "moving"}
                />
              </label>

              {validation && !validation.error && validation.ok && (
                <p className="settings-note">
                  Ready to move {fmtBytes(validation.treeBytes ?? 0)}
                  {validation.freeBytes != null && ` — ${fmtBytes(validation.freeBytes)} free at target`}
                  {validation.sameFilesystem ? " (same disk, instant)" : ""}.
                </p>
              )}
              {validation && validation.ok === false && validation.error && (
                <div className="error">{validation.error}</div>
              )}
              {error && <div className="error">{error}</div>}

              {move.kind === "moving" && (
                <ProgressBar
                  value={move.copied}
                  max={move.total}
                  label={`${move.phase.charAt(0).toUpperCase()}${move.phase.slice(1)}…`}
                  caption={
                    move.total > 0 ? (
                      <span className="mono">
                        {fmtBytes(move.copied)} / {fmtBytes(move.total)}
                      </span>
                    ) : undefined
                  }
                />
              )}
              {move.kind === "done" && (
                <p className="settings-note">
                  Moved. orx is now using the new location.
                  {move.oldPathLeft && (
                    <>
                      {" "}
                      The old copy was left at <code>{move.oldPathLeft}</code> (different disk) — you
                      can delete it once you&apos;ve confirmed everything works.
                    </>
                  )}
                </p>
              )}
              {move.kind === "error" && <div className="error">Move failed: {move.message}</div>}

              <div className="actions">
                <button
                  type="button"
                  className="btn"
                  onClick={check}
                  disabled={checking || !trimmed || unchanged || move.kind === "moving"}
                >
                  {checking ? "Checking…" : "Check"}
                </button>
                <button
                  type="submit"
                  className="btn primary"
                  disabled={!trimmed || unchanged || move.kind === "moving"}
                >
                  {move.kind === "moving" ? "Moving…" : "Move data here"}
                </button>
              </div>
            </form>
          )}
        </div>
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
                  <StatusBadge status={runDisplayStatus(inst)} />
                </td>
                <td>{timeAgo(inst.createdAt)}</td>
                <td>{runtimeLabel(inst)}</td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

function ComputeActivity({ onViewHistory }: { onViewHistory: () => void }) {
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

  // Point-in-time snapshot: the page refetches on every open, and this button
  // refreshes in place while sitting on it — the run.updated
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
    <section className="compute-activity">
      <div className="compute-activity-head">
        <div>
          <h2>
            Running instances
            {running && running.length > 0 && <span className="count-badge">{running.length}</span>}
          </h2>
          <p>Compute currently active across all projects.</p>
        </div>
        <div className="compute-activity-actions">
          <button className="btn sm" onClick={load} disabled={refreshing}>
            <RefreshCw size={12} className={refreshing ? "spin" : ""} /> Refresh
          </button>
          <button className="btn sm" onClick={onViewHistory}>
            {`View history${past?.length ? ` (${past.length})` : ""}`}
          </button>
        </div>
      </div>
      {error && <div className="error">{error}</div>}
      {!running || !past ? (
        <div className="settings-loading">
          <span className="spinner" /> Loading…
        </div>
      ) : <InstancesTable instances={running} emptyLabel="Nothing running right now." />}
    </section>
  );
}

function InstanceHistory({ onBack }: { onBack: () => void }) {
  const [instances, setInstances] = useState<Instance[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const [, setTick] = useState(0);

  useEffect(() => {
    const timer = setInterval(() => setTick((tick) => tick + 1), 30_000);
    return () => clearInterval(timer);
  }, []);

  const load = () => {
    setRefreshing(true);
    listInstances()
      .then((rows) => {
        setInstances(rows.sort((a, b) => b.createdAt - a.createdAt));
        setError(null);
      })
      .catch((err) => {
        setError(err instanceof Error ? err.message : String(err));
        setInstances((current) => current ?? []);
      })
      .finally(() => setRefreshing(false));
  };
  useEffect(load, []);

  return (
    <>
      <button type="button" className="settings-back" onClick={onBack}>
        <ArrowLeft size={14} /> Back to Compute
      </button>
      <div className="settings-head-row">
        <h1>Instance history</h1>
        <button className="btn sm" onClick={load} disabled={refreshing}>
          <RefreshCw size={12} className={refreshing ? "spin" : ""} /> Refresh
        </button>
      </div>
      <p className="settings-sub">Every compute instance spun up across your projects.</p>
      {error && <div className="error">{error}</div>}
      {!instances ? (
        <div className="settings-loading"><span className="spinner" /> Loading…</div>
      ) : (
        <InstancesTable instances={instances} emptyLabel="No instances yet." />
      )}
    </>
  );
}

// --- embedded view -----------------------------------------------------------

type SettingsNavItem = {
  id: Tab;
  label: string;
  icon: React.ReactNode;
  activeTabs: Tab[];
};

const SETTINGS_SECTIONS: Tab[] = ["projects", "harnesses", "storage"];

/** Primary rail entries. Configuration sections share the Settings entry. */
export const SETTINGS_NAV: SettingsNavItem[] = [
  {
    id: "compute",
    label: "Compute",
    icon: <Cpu size={15} />,
    activeTabs: ["compute", "instances"],
  },
  { id: "environment", label: "Environment", icon: <SquareTerminal size={15} />, activeTabs: ["environment"] },
  {
    id: "settings",
    label: "Settings",
    icon: <Settings size={15} />,
    activeTabs: ["settings", ...SETTINGS_SECTIONS],
  },
];

function isSettingsSection(tab: Tab): boolean {
  return SETTINGS_SECTIONS.includes(tab);
}

/** One settings section's content, shown in the middle pane in place of chat. */
export function SettingsView({
  tab,
  project,
  onSelectTab,
}: {
  tab: Tab;
  project: Project | null;
  onSelectTab: (tab: Tab) => void;
}) {
  const showsSettings = tab === "settings" || isSettingsSection(tab);

  return (
    <div className="settings-view">
      {showsSettings && (
        <>
          <h1>Settings</h1>
          <div className="settings-stack">
            <section className="settings-stack-section">
              <AppearanceTab />
            </section>
            <section className="settings-stack-section">
              <HarnessesTab />
            </section>
            <section className="settings-stack-section">
              <StorageTab />
            </section>
          </div>
        </>
      )}
      {tab === "compute" && (
        <ComputeTab
          project={project}
          onViewHistory={() => onSelectTab("instances")}
        />
      )}
      {tab === "instances" && <InstanceHistory onBack={() => onSelectTab("compute")} />}
      {tab === "environment" && (
        <>
          <h1>Environment</h1>
          <p className="settings-sub">
            Variables available to runs and the research agent (API keys, tokens).
          </p>
          <EnvVarsSection />
        </>
      )}
      {tab === "git" && <GitTab project={project} />}
    </div>
  );
}
