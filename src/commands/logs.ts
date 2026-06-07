import { readRunLog } from "../client.ts";
import { requireCredentials } from "../config.ts";

interface LogsOptions {
  head?: boolean;
  bytes?: string;
  range?: string;
}

/**
 * Prints a run's terminal log. Tail by default (the end is usually what you
 * want); `--head` reads from the start, `--range <start>:<end>` an exact byte
 * window (offsets come from `orx search-logs`).
 */
export async function logs(runId: string | undefined, options: LogsOptions): Promise<void> {
  if (!runId) {
    console.error("Usage: orx logs <runId> [--head] [--bytes <n>] [--range <start>:<end>]");
    process.exit(1);
  }
  const creds = await requireCredentials();

  let mode: "head" | "tail" | "range" = options.head ? "head" : "tail";
  let startByte: number | undefined;
  let endByte: number | undefined;

  if (options.range) {
    const [s, e] = options.range.split(":");
    startByte = Number(s);
    endByte = Number(e);
    if (!Number.isInteger(startByte) || !Number.isInteger(endByte) || endByte <= startByte) {
      console.error("--range must be <start>:<end> byte offsets with end > start.");
      process.exit(1);
    }
    mode = "range";
  }

  const maxBytes = options.bytes ? Number(options.bytes) : undefined;
  if (options.bytes && !Number.isInteger(maxBytes)) {
    console.error("--bytes must be an integer.");
    process.exit(1);
  }

  const log = await readRunLog(creds, runId, { mode, maxBytes, startByte, endByte });

  // The log itself goes to stdout (pipe-friendly); metadata to stderr so it
  // doesn't pollute a `| grep` or a redirect.
  process.stdout.write(log.content);
  if (log.content && !log.content.endsWith("\n")) process.stdout.write("\n");

  const span = `bytes ${log.startByte}–${log.endByte} of ${log.totalBytes}`;
  const more = [
    log.truncatedBefore ? "more above" : null,
    log.truncatedAfter ? "more below" : null,
  ].filter(Boolean);
  console.error(
    `[${log.source}] ${span}${more.length ? ` (${more.join(", ")})` : ""}`,
  );
}
