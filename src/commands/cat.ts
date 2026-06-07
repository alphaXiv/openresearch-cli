import { readWorkdir } from "../client.ts";
import { requireCredentials } from "../config.ts";

/**
 * Prints a file's content from an experiment's committed branch. Reads Forgejo
 * directly — no open dev node required.
 */
export async function cat(
  expId: string | undefined,
  path: string | undefined,
): Promise<void> {
  if (!expId || !path) {
    console.error("Usage: orx cat <experimentId> <path>");
    process.exit(1);
  }
  const creds = await requireCredentials();
  const { content } = await readWorkdir(creds, expId, path);
  process.stdout.write(content);
  if (content && !content.endsWith("\n")) process.stdout.write("\n");
}
