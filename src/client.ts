import type { Credentials } from "./config.ts";

/** Minimal shapes of the API responses this CLI consumes. */
export interface Org {
  id: string;
  name: string;
  createdBy: string;
}

export interface Project {
  id: string;
  name: string;
  description: string;
  archived: boolean;
}

export interface Experiment {
  id: string;
  projectId: string;
  parentExperimentId: string | null;
  slug: string;
  title: string;
  status: string;
  runCommand: string;
  updatedAt: string;
}

export interface Run {
  id: string;
  experimentId: string;
  command: string;
  status: string;
  commitSha: string | null;
  updatedAt: string;
}

export interface ProjectQueryResult {
  columns: string[];
  rows: unknown[][];
  rowCount: number;
  totalRowCount: number;
  moreRowsAvailable: boolean;
  syncStatus: "degraded" | "ready" | "warming";
  syncErrors: string[];
  lastSyncedAt: string | null;
}

async function request<T>(
  creds: Credentials,
  path: string,
  init?: { method: string; body: unknown },
): Promise<T> {
  let res: Response;
  try {
    res = await fetch(`${creds.apiUrl}${path}`, {
      method: init?.method ?? "GET",
      headers: {
        Authorization: `Bearer ${creds.token}`,
        ...(init?.body !== undefined ? { "content-type": "application/json" } : {}),
      },
      body: init?.body !== undefined ? JSON.stringify(init.body) : undefined,
    });
  } catch (err) {
    throw new Error(`Could not reach the API at ${creds.apiUrl}: ${String(err)}`);
  }

  if (res.status === 401) {
    throw new Error("Unauthorized — your token is invalid or revoked. Run `orx login` again.");
  }
  if (!res.ok) {
    // Surface the server's error message when it sends one.
    const detail = await res.text().catch(() => "");
    const suffix = detail ? `: ${detail}` : "";
    throw new Error(`Request to ${path} failed (${res.status} ${res.statusText})${suffix}`);
  }
  return (await res.json()) as T;
}

function apiGet<T>(creds: Credentials, path: string): Promise<T> {
  return request<T>(creds, path);
}

export function listOrgs(creds: Credentials): Promise<{ orgs: Org[] }> {
  return apiGet(creds, "/orgs");
}

export function listProjects(
  creds: Credentials,
  orgId: string,
): Promise<{ projects: Project[] }> {
  return apiGet(creds, `/orgs/${orgId}/projects`);
}

export function listExperiments(
  creds: Credentials,
  projectId: string,
): Promise<{ experiments: Experiment[] }> {
  return apiGet(creds, `/projects/${projectId}/experiments`);
}

export function listRuns(creds: Credentials, projectId: string): Promise<{ runs: Run[] }> {
  return apiGet(creds, `/projects/${projectId}/runs`);
}

export function queryProject(
  creds: Credentials,
  projectId: string,
  sql: string,
): Promise<ProjectQueryResult> {
  return request(creds, `/projects/${projectId}/query`, { method: "POST", body: { sql } });
}

export interface WandbChartResult {
  /** Null when no run produced any points (nothing was rendered). */
  chartId: string | null;
  /** Presigned PNG URL, or null when nothing was rendered. */
  url: string | null;
  metricKey: string;
  summaries: { label: string; n: number; min: number; max: number; last: number }[];
  failed: { label: string; error: string }[];
}

export function renderWandbChart(
  creds: Credentials,
  projectId: string,
  body: { metricKey: string; runs: { runId: string; label?: string }[]; smoothing?: number },
): Promise<WandbChartResult> {
  return request(creds, `/projects/${projectId}/charts/wandb`, { method: "POST", body });
}

export function createChildExperiment(
  creds: Credentials,
  projectId: string,
  body: { title: string; description?: string; parentExperimentId: string },
): Promise<{ experiment: Experiment }> {
  return request(creds, `/projects/${projectId}/experiments`, { method: "POST", body });
}

export function createEmptyBaseline(
  creds: Credentials,
  projectId: string,
  body: { title: string; description?: string },
): Promise<{ experiment: Experiment }> {
  return request(creds, `/projects/${projectId}/create-empty-baseline`, { method: "POST", body });
}

export function importBaseline(
  creds: Credentials,
  projectId: string,
  body: { repoFullName: string; ref: string; patch: null; title: string; description?: string },
): Promise<{ experiment: Experiment }> {
  return request(creds, `/projects/${projectId}/import-baseline`, { method: "POST", body });
}

export interface DevSession {
  state: "none" | "provisioning" | "online" | "offline";
  sandboxId: string | null;
}

export interface DevStatus extends DevSession {
  dirty: string[];
}

export type DevFsOp =
  | { op: "read"; path: string }
  | { op: "write"; path: string; content: string }
  | { op: "str_replace"; path: string; old_string: string; new_string: string }
  | { op: "list"; path?: string }
  | { op: "search"; query: string }
  | { op: "delete"; path: string };

export function devOpen(creds: Credentials, expId: string): Promise<DevSession> {
  return request(creds, `/experiments/${expId}/dev/open`, { method: "POST", body: {} });
}

export function devStatus(creds: Credentials, expId: string): Promise<DevStatus> {
  return apiGet(creds, `/experiments/${expId}/dev/status`);
}

export function devClose(
  creds: Credentials,
  expId: string,
  body: { message?: string; discard?: boolean },
): Promise<{ committed: boolean; commitSha: string | null; tornDown: boolean }> {
  return request(creds, `/experiments/${expId}/dev/close`, { method: "POST", body });
}

export function devFs(creds: Credentials, expId: string, op: DevFsOp): Promise<{ output: string }> {
  return request(creds, `/experiments/${expId}/dev/fs`, { method: "POST", body: op });
}

export interface RunLogExcerpt {
  content: string;
  startByte: number;
  endByte: number;
  totalBytes: number;
  source: string;
  truncatedBefore: boolean;
  truncatedAfter: boolean;
}

export interface LogSearchResult {
  capped: boolean;
  pattern: string;
  results: {
    runId: string;
    matchCount: number;
    totalLines: number;
    source: string;
    matchingLines: { lineNumber: number; startByte: number; endByte: number; text: string }[];
  }[];
}

export function readRunLog(
  creds: Credentials,
  runId: string,
  opts: { mode?: "head" | "tail" | "range"; maxBytes?: number; startByte?: number; endByte?: number },
): Promise<RunLogExcerpt> {
  const params = new URLSearchParams();
  if (opts.mode) params.set("mode", opts.mode);
  if (opts.maxBytes != null) params.set("maxBytes", String(opts.maxBytes));
  if (opts.startByte != null) params.set("startByte", String(opts.startByte));
  if (opts.endByte != null) params.set("endByte", String(opts.endByte));
  const qs = params.toString();
  return apiGet(creds, `/runs/${runId}/log${qs ? `?${qs}` : ""}`);
}

export function searchLogs(
  creds: Credentials,
  projectId: string,
  body: { pattern: string; runId?: string; experimentId?: string; maxMatchingLines?: number },
): Promise<LogSearchResult> {
  return request(creds, `/projects/${projectId}/search-logs`, { method: "POST", body });
}

// Committed-workdir reads. Unlike the dev/fs ops, these read the experiment's
// Forgejo branch directly, so they work without an open dev node.

export function searchWorkdir(
  creds: Credentials,
  expId: string,
  query: string,
): Promise<{ output: string }> {
  return apiGet(creds, `/experiments/${expId}/workdir/search?q=${encodeURIComponent(query)}`);
}

export function lsWorkdir(
  creds: Credentials,
  expId: string,
  path?: string,
): Promise<{ files: { path: string; size: number | null }[] }> {
  const qs = path ? `?path=${encodeURIComponent(path)}` : "";
  return apiGet(creds, `/experiments/${expId}/workdir/ls${qs}`);
}

export function readWorkdir(
  creds: Credentials,
  expId: string,
  path: string,
): Promise<{ content: string }> {
  return apiGet(creds, `/experiments/${expId}/workdir/read?path=${encodeURIComponent(path)}`);
}

export interface ArtifactExcerpt {
  content: string;
  key: string;
  startByte: number;
  endByte: number;
  totalBytes: number;
  truncatedBefore: boolean;
  truncatedAfter: boolean;
}

export function readArtifact(
  creds: Credentials,
  runId: string,
  key: string,
  opts: { mode?: "head" | "tail"; maxBytes?: number },
): Promise<ArtifactExcerpt> {
  const params = new URLSearchParams({ key });
  if (opts.mode) params.set("mode", opts.mode);
  if (opts.maxBytes != null) params.set("maxBytes", String(opts.maxBytes));
  return apiGet(creds, `/runs/${runId}/artifact?${params.toString()}`);
}

export interface SkillRef {
  name: string;
  description: string;
  path: string;
}

export function listSkills(creds: Credentials): Promise<{ skills: SkillRef[] }> {
  return apiGet(creds, "/skills");
}

export function readSkill(creds: Credentials, path: string): Promise<{ content: string }> {
  return apiGet(creds, `/skills/read?path=${encodeURIComponent(path)}`);
}
