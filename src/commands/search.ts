import { searchWorkdir } from "../client.ts";
import { requireCredentials } from "../config.ts";

/**
 * Greps an experiment's committed branch for a case-insensitive substring.
 * Reads Forgejo directly, so unlike `orx grep` it needs no open dev node.
 */
export async function search(
  expId: string | undefined,
  query: string | undefined,
): Promise<void> {
  if (!expId || !query) {
    console.error('Usage: orx search <experimentId> "<query>"');
    process.exit(1);
  }
  const creds = await requireCredentials();
  const { output } = await searchWorkdir(creds, expId, query);
  console.log(output);
}
