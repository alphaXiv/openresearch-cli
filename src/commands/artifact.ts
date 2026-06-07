import { readArtifact } from "../client.ts";
import { requireCredentials } from "../config.ts";

interface ArtifactOptions {
  head?: boolean;
  bytes?: string;
}

/**
 * Prints a bounded excerpt of a run's text artifact. Tail by default; `--head`
 * reads from the start. Reading also caches the excerpt server-side, so it
 * becomes searchable afterwards via:
 *   orx query <projectId> "select run_id, key, content from artifact_text_excerpts where content ilike '%...%'"
 * Artifact keys come from the artifacts table (`orx query ... from artifacts`).
 */
export async function artifact(
  runId: string | undefined,
  key: string | undefined,
  options: ArtifactOptions,
): Promise<void> {
  if (!runId || !key) {
    console.error("Usage: orx artifact <runId> <key> [--head] [--bytes <n>]");
    process.exit(1);
  }
  const creds = await requireCredentials();

  const mode: "head" | "tail" = options.head ? "head" : "tail";
  const maxBytes = options.bytes ? Number(options.bytes) : undefined;
  if (options.bytes && !Number.isInteger(maxBytes)) {
    console.error("--bytes must be an integer.");
    process.exit(1);
  }

  const a = await readArtifact(creds, runId, key, { mode, maxBytes });

  // Content to stdout (pipe-friendly); metadata to stderr.
  process.stdout.write(a.content);
  if (a.content && !a.content.endsWith("\n")) process.stdout.write("\n");

  const span = `bytes ${a.startByte}–${a.endByte} of ${a.totalBytes}`;
  const more = [
    a.truncatedBefore ? "more above" : null,
    a.truncatedAfter ? "more below" : null,
  ].filter(Boolean);
  console.error(`[${a.key}] ${span}${more.length ? ` (${more.join(", ")})` : ""}`);
}
