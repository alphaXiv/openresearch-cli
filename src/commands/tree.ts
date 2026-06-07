import { lsWorkdir } from "../client.ts";
import { requireCredentials } from "../config.ts";

/**
 * Lists files in an experiment's committed branch under an optional path.
 * Reads Forgejo directly — no open dev node required.
 */
export async function tree(
  expId: string | undefined,
  path: string | undefined,
): Promise<void> {
  if (!expId) {
    console.error("Usage: orx tree <experimentId> [path]");
    process.exit(1);
  }
  const creds = await requireCredentials();
  const { files } = await lsWorkdir(creds, expId, path);
  if (files.length === 0) {
    console.error("No files.");
    return;
  }
  for (const file of files) console.log(file.path);
}
