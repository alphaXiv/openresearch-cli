// Typed client for the orx up local HTTP API (/api/*). All wire JSON is camelCase.

export interface Project {
  id: string;
  name: string;
  slug: string;
  githubOwner: string;
  githubRepo: string;
  baselineBranch: string;
  repoPath: string;
  runCommand?: string | null;
  createdAt: number;
  updatedAt: number;
}

export interface Experiment {
  id: string;
  projectId: string;
  parentExperimentId?: string | null;
  slug: string;
  branchName: string;
  title?: string | null;
  description?: string | null;
  runCommand: string;
  agentStatus: string;
  createdAt: number;
  updatedAt: number;
}

export type RunStatus = "starting" | "running" | "done" | "failed" | "cancelled";

export interface Run {
  id: string;
  experimentId: string;
  projectId: string;
  status: RunStatus;
  backend?: Record<string, unknown> | null;
  command?: string | null;
  commitSha?: string | null;
  resultMarkdown?: string | null;
  createdAt: number;
  updatedAt: number;
  endedAt?: number | null;
  exitCode?: number | null;
}

async function json<T>(res: Response): Promise<T> {
  if (!res.ok) {
    const text = await res.text().catch(() => "");
    let message = text;
    try {
      const parsed = JSON.parse(text) as { error?: string };
      if (parsed.error) message = parsed.error;
    } catch {
      // non-JSON body — show it raw
    }
    throw new Error(message || `HTTP ${res.status}`);
  }
  return (await res.json()) as T;
}

const get = <T>(url: string) => fetch(url).then((r) => json<T>(r));
const post = <T>(url: string, body?: unknown) =>
  fetch(url, {
    method: "POST",
    headers: body === undefined ? {} : { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  }).then((r) => json<T>(r));
const patch = <T>(url: string, body: unknown) =>
  fetch(url, {
    method: "PATCH",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  }).then((r) => json<T>(r));

export const listProjects = () =>
  get<{ projects: Project[] }>("/api/projects").then((r) => r.projects);

export interface NewProject {
  name: string;
  githubOwner?: string;
  githubRepo?: string;
  baselineBranch?: string;
  runCommand?: string;
  /** Create a blank private repo on the user's GitHub account instead. */
  createRepo?: boolean;
}

export const createProject = (body: NewProject) =>
  post<{ project: Project }>("/api/projects", body).then((r) => r.project);

export const updateProject = (projectId: string, body: { runCommand?: string; name?: string }) =>
  patch<{ project: Project }>(`/api/projects/${projectId}`, body).then((r) => r.project);

export const listExperiments = (projectId: string) =>
  get<{ experiments: Experiment[] }>(`/api/projects/${projectId}/experiments`).then(
    (r) => r.experiments,
  );

export const listRuns = (projectId: string) =>
  get<{ runs: Run[] }>(`/api/projects/${projectId}/runs`).then((r) => r.runs);

export interface NewExperiment {
  parentExperimentId: string;
  slug: string;
  title?: string;
  description?: string;
  runCommand?: string;
}

export const createExperiment = (projectId: string, body: NewExperiment) =>
  post<{ experiment: Experiment }>(`/api/projects/${projectId}/experiments`, body).then(
    (r) => r.experiment,
  );

export const startRun = (
  experimentId: string,
  body: { backend?: "hf" | "k8s"; flavor?: string; manifest?: string; timeout?: string } = {},
) => post<{ run: Run }>(`/api/experiments/${experimentId}/run`, body).then((r) => r.run);

export const cancelRun = (runId: string) => post<{ ok: boolean }>(`/api/runs/${runId}/cancel`);

export interface LogChunk {
  dataBase64: string;
  nextOffset: number;
  eof: boolean;
}

export const fetchLog = (runId: string, offset: number) =>
  get<LogChunk>(`/api/runs/${runId}/log?offset=${offset}`);

export interface DiffPayload {
  diff: string;
  truncated: boolean;
  bytesRead: number;
  byteLimit: number;
}

export interface CommitInfo {
  sha: string;
  subject: string;
  committedAt: number; // unix seconds
}

export interface WorkingTree {
  branch: string | null;
  experimentId: string | null;
  diff: string;
  truncated: boolean;
}

export const getRunDiff = (runId: string) => get<DiffPayload>(`/api/runs/${runId}/diff`);

export const listExperimentCommits = (experimentId: string) =>
  get<{ commits: CommitInfo[] }>(`/api/experiments/${experimentId}/commits`).then(
    (r) => r.commits,
  );

export const getCommitDiff = (experimentId: string, sha: string) =>
  get<DiffPayload>(`/api/experiments/${experimentId}/commits/${sha}/diff`);

export const getWorkingTree = (projectId: string) =>
  get<WorkingTree>(`/api/projects/${projectId}/working-tree`);

export interface ProjectFile {
  path: string;
  content: string;
  truncated: boolean;
  notFound: boolean;
}

/** One file from the project clone, capped server-side (~512 KB). */
export const getProjectFile = (projectId: string, path: string) =>
  get<ProjectFile>(`/api/projects/${projectId}/file?path=${encodeURIComponent(path)}`);

export type HfTokenSource = "env" | "openresearchEnv" | "hfCache";

export interface HfSettings {
  configured: boolean;
  source: HfTokenSource | null;
  maskedToken: string | null;
  valid: boolean;
  username: string | null;
  jobsWrite: boolean | null;
}

export const getHfSettings = () => get<HfSettings>("/api/settings/hf");

export const saveHfToken = (token: string) => post<HfSettings>("/api/settings/hf", { token });

// --- settings: kubernetes -----------------------------------------------------

export interface K8sPreflight {
  kubectlFound: boolean;
  reachable: boolean;
  canCreateJobs: boolean;
  error?: string;
}

export interface K8sSettings {
  configured: boolean;
  contexts: string[];
  currentContext: string | null;
  context: string | null;
  namespace: string;
  preflight: K8sPreflight;
}

export const getK8sSettings = () => get<K8sSettings>("/api/settings/k8s");

export const saveK8sSettings = (body: { context?: string; namespace?: string }) =>
  post<K8sSettings>("/api/settings/k8s", body);

// --- settings: modal ----------------------------------------------------------

export type ModalTokenSource = "env" | "syncedEnv" | "modalToml";

export interface ModalSettings {
  /** The orx-managed venv exists on disk. */
  envProvisioned: boolean;
  /** `import modal` succeeds with the resolved interpreter. */
  modalImportable: boolean;
  tokenConfigured: boolean;
  tokenSource: ModalTokenSource | null;
  /** modalImportable && tokenConfigured. */
  ready: boolean;
  error: string | null;
}

export const getModalSettings = () => get<ModalSettings>("/api/settings/modal");

/** Build the orx-managed Modal env (first run downloads the SDK, ~30–60s). */
export const provisionModal = () => post<ModalSettings>("/api/settings/modal/provision");

// --- settings: env vars / git / harnesses ------------------------------------

export interface EnvVar {
  key: string;
  maskedValue: string;
  inProcessEnv: boolean;
}

export const getEnvVars = () =>
  get<{ vars: EnvVar[] }>("/api/settings/env").then((r) => r.vars);

export const setEnvVar = (key: string, value: string) =>
  post<{ vars: EnvVar[] }>("/api/settings/env", { key, value }).then((r) => r.vars);

export const deleteEnvVar = (key: string) =>
  fetch(`/api/settings/env/${encodeURIComponent(key)}`, { method: "DELETE" })
    .then((r) => json<{ vars: EnvVar[] }>(r))
    .then((r) => r.vars);

export interface SshHost {
  host: string;
  hostname?: string;
  user?: string;
  port?: string;
  identityFile?: string;
}

export const getSshHosts = () =>
  get<{ hosts: SshHost[] }>("/api/settings/ssh").then((r) => r.hosts);

export interface SshPreflight {
  reachable: boolean;
  gitFound: boolean;
  error: string | null;
}

/** Live-test a host: reachable over ssh (BatchMode) and has `git`. */
export const sshPreflight = (host: string) =>
  post<SshPreflight>("/api/settings/ssh/preflight", { host });

/** A subfolder of the artifacts dir with a top-level report.md. */
export interface ArtifactReport {
  name: string;
  title: string;
  modifiedAt: number;
}

/** Any other file in the artifacts dir, by dir-relative path. */
export interface ArtifactFile {
  path: string;
  size: number;
  modifiedAt: number;
}

/** Listing of the project's on-disk artifacts directory. */
export interface Artifacts {
  dir: string;
  reports: ArtifactReport[];
  files: ArtifactFile[];
  truncated: boolean;
}

export const getArtifacts = (projectId: string) =>
  get<Artifacts>(`/api/projects/${projectId}/artifacts`);

export const getArtifactReport = (projectId: string, name: string) =>
  get<{ markdown: string }>(
    `/api/projects/${projectId}/artifacts/report?path=${encodeURIComponent(name)}`,
  );

/** Delete a file or report folder in the artifacts dir. */
export const deleteArtifact = (projectId: string, path: string) =>
  fetch(`/api/projects/${projectId}/artifacts?path=${encodeURIComponent(path)}`, {
    method: "DELETE",
  }).then((r) => json<{ ok: boolean }>(r));

/** Raw artifact file (images, CSVs, report figures) served by the API. */
export const artifactFileUrl = (projectId: string, path: string) =>
  `/api/projects/${projectId}/artifacts/file?path=${encodeURIComponent(path)}`;

export interface GitSettings {
  gitVersion: string | null;
  userName: string | null;
  userEmail: string | null;
  ghInstalled: boolean;
  githubTokenSource: "env" | "stored" | "gh" | null;
}

export const getGitSettings = () => get<GitSettings>("/api/settings/git");

export const saveGitSettings = (body: { userName?: string; userEmail?: string }) =>
  post<GitSettings>("/api/settings/git", body);

/** Validate + persist a pasted GitHub token (stored in the synced env file). */
export const saveGitToken = (token: string) =>
  post<GitSettings>("/api/settings/git/token", { token });

export const removeGitToken = () =>
  fetch("/api/settings/git/token", { method: "DELETE" }).then((r) => json<GitSettings>(r));

export type HarnessId = "claude-code" | "codex" | "opencode";

export interface HarnessModel {
  id: string;
}

export interface Harness {
  id: HarnessId;
  name: string;
  installed: boolean;
  binPath?: string;
  version?: string;
  authenticated: boolean;
  authMethod?: "oauth" | "apiKey";
  account?: string;
  org?: string;
  plan?: string;
  agentReady: boolean;
  agentNote?: string;
  models: HarnessModel[];
}

export const getHarnesses = (refresh = false) =>
  get<{ harnesses: Harness[] }>(`/api/harnesses${refresh ? "?refresh=1" : ""}`).then(
    (r) => r.harnesses,
  );

/** "openai/gpt-5.5" → "GPT 5.5", "anthropic/claude-opus-4-8" → "Opus 4.8". */
export function modelLabel(id: string): string {
  const last = (id.split("/").pop() ?? id).replace(/^~/, "").replace(/^claude-/, "");
  const words: string[] = [];
  const nums: string[] = [];
  for (const part of last.split("-")) {
    if (/^\d+(\.\d+)?$/.test(part)) {
      nums.push(part);
    } else {
      if (nums.length) words.push(nums.splice(0).join("."));
      words.push(part === "gpt" ? "GPT" : part.charAt(0).toUpperCase() + part.slice(1));
    }
  }
  if (nums.length) words.push(nums.join("."));
  return words.join(" ");
}

// --- chat (unified harness sessions) ------------------------------------------

export interface ChatToolState {
  status: "running" | "completed" | "error";
  input?: { command?: string; filePath?: string; description?: string; [k: string]: unknown };
  output?: string;
  error?: string;
  title?: string;
}

export interface ChatPart {
  id: string;
  type: string; // text | reasoning | tool
  text?: string;
  tool?: string;
  state?: ChatToolState;
}

export interface ChatMessage {
  id: string;
  role: "user" | "assistant";
  parts: ChatPart[];
  createdAt: number;
}

export interface ChatSession {
  id: string;
  projectId: string;
  harness: HarnessId;
  title: string | null;
  model: string | null;
  createdAt: number;
  updatedAt: number;
  busy: boolean;
}

export const listChatSessions = (projectId: string) =>
  get<{ sessions: ChatSession[] }>(
    `/api/chat/sessions?projectId=${encodeURIComponent(projectId)}`,
  ).then((r) => r.sessions);

export const createChatSession = (projectId: string, harness: HarnessId, model?: string | null) =>
  post<{ session: ChatSession }>("/api/chat/sessions", { projectId, harness, model }).then(
    (r) => r.session,
  );

export const deleteChatSession = (sessionId: string) =>
  fetch(`/api/chat/sessions/${sessionId}`, { method: "DELETE" }).then((r) => r.ok);

export const getChatMessages = (sessionId: string) =>
  get<{ messages: ChatMessage[] }>(`/api/chat/sessions/${sessionId}/messages`).then(
    (r) => r.messages,
  );

/** Returns immediately; the turn streams over /api/events (chat.* events). */
export const sendChatMessage = (sessionId: string, text: string, model?: string | null) =>
  post<{ ok: boolean }>(`/api/chat/sessions/${sessionId}/message`, { text, model });

export const interruptChat = (sessionId: string) =>
  post<{ ok: boolean }>(`/api/chat/sessions/${sessionId}/interrupt`);

// --- helpers shared across views --------------------------------------------

export function statusColor(status: string): string {
  switch (status) {
    case "done":
      return "var(--green)";
    case "running":
      return "var(--teal)";
    case "starting":
      return "var(--amber)";
    case "failed":
      return "var(--red)";
    case "cancelled":
      return "var(--muted)";
    default:
      return "var(--muted)";
  }
}

export function timeAgo(ms: number): string {
  const s = Math.max(0, Math.floor((Date.now() - ms) / 1000));
  if (s < 60) return `${s}s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  return `${Math.floor(h / 24)}d ago`;
}

export function shortId(id: string): string {
  return id.length > 10 ? `${id.slice(0, 10)}…` : id;
}
