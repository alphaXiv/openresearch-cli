import { searchLogs } from "../client.ts";
import { requireCredentials } from "../config.ts";

interface SearchLogsOptions {
  run?: string;
  experiment?: string;
  max?: string;
}

/**
 * Greps a project's run logs for a literal pattern. Scope to one run with
 * `--run`, or one experiment's runs with `--experiment` (one is required).
 */
export async function searchLogsCommand(
  projectId: string | undefined,
  pattern: string | undefined,
  options: SearchLogsOptions,
): Promise<void> {
  if (!projectId || !pattern) {
    console.error(
      'Usage: orx search-logs <projectId> "<pattern>" (--run <id> | --experiment <id>) [--max <n>]',
    );
    process.exit(1);
  }
  if (!options.run && !options.experiment) {
    console.error("Provide --run <id> or --experiment <id> to scope the search.");
    process.exit(1);
  }
  const creds = await requireCredentials();

  const { capped, results } = await searchLogs(creds, projectId, {
    pattern,
    runId: options.run,
    experimentId: options.experiment,
    maxMatchingLines: options.max ? Number(options.max) : undefined,
  });

  let total = 0;
  for (const run of results) {
    if (run.matchingLines.length === 0) continue;
    // grep-style: short run id, line number, then the matching line. Byte
    // offsets are tucked at the end for feeding back into `orx logs --range`.
    for (const m of run.matchingLines) {
      console.log(`${run.runId.slice(0, 8)}:${m.lineNumber}: ${m.text}  ← ${m.startByte}:${m.endByte}`);
      total++;
    }
  }

  if (total === 0) {
    console.error("No matches.");
    return;
  }
  console.error(`\n${total} matching line(s)${capped ? " (capped — narrow the search or raise --max)" : ""}.`);
}
