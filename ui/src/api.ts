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

export interface AgentStatus {
  running: boolean;
  port?: number | null;
  projectId?: string | null;
  model?: string | null;
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
  githubOwner: string;
  githubRepo: string;
  baselineBranch?: string;
  runCommand?: string;
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

export const startRun = (experimentId: string, body: { flavor?: string; timeout?: string } = {}) =>
  post<{ run: Run }>(`/api/experiments/${experimentId}/run`, body).then((r) => r.run);

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

export const getAgentStatus = () => get<AgentStatus>("/api/agent/status");

export const ensureAgent = (projectId: string) =>
  post<{ running: boolean; port?: number }>("/api/agent/ensure", { projectId });

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
