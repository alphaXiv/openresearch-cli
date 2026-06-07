import { listExperiments, listRuns } from "../client.ts";
import { requireCredentials } from "../config.ts";
import { printTable } from "../table.ts";

interface RunsOptions {
  experimentId?: string;
}

/** Lists a project's runs as a table, newest first. */
export async function runs(projectId: string | undefined, options: RunsOptions): Promise<void> {
  if (!projectId) {
    console.error("Usage: orx runs <projectId> [--experiment <experimentId>]");
    process.exit(1);
  }
  const creds = await requireCredentials();

  // Fetch experiments too, so we can label each run with its experiment title
  // rather than a bare id.
  const [{ runs }, { experiments }] = await Promise.all([
    listRuns(creds, projectId),
    listExperiments(creds, projectId),
  ]);
  const titleOf = new Map(experiments.map((e) => [e.id, e.title]));

  const filtered = options.experimentId
    ? runs.filter((r) => r.experimentId === options.experimentId)
    : runs;
  // Run ids are UUIDv7 — lexicographic sort is chronological. Newest first.
  const sorted = filtered.toSorted((a, b) => b.id.localeCompare(a.id));

  if (sorted.length === 0) {
    console.log("No runs found.");
    return;
  }

  printTable(
    ["STATUS", "EXPERIMENT", "COMMIT", "UPDATED"],
    sorted.map((r) => [
      r.status,
      titleOf.get(r.experimentId) ?? r.experimentId,
      r.commitSha ? r.commitSha.slice(0, 7) : "—",
      r.updatedAt,
    ]),
  );
}
