import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { listSkills, readSkill } from "../client.ts";
import { loadCredentials, requireCredentials } from "../config.ts";

// Bundled top-level overview, shipped with the CLI so `orx skill` works without
// a round-trip. Deeper references are fetched live from the API.
const SKILL_MD_PATH = join(import.meta.dirname, "..", "..", "SKILL.md");

export async function skill(path: string | undefined): Promise<void> {
  // With a path: fetch the canonical doc from the API (same docs the assistant
  // reads), so the schema never drifts from a hand-maintained copy.
  if (path) {
    const creds = await requireCredentials();
    const { content } = await readSkill(creds, path);
    console.log(content);
    return;
  }

  // No path: print the bundled overview, then list fetchable skills (best
  // effort — skip the index if we can't reach the API).
  const overview = await readFile(SKILL_MD_PATH, "utf8");
  console.log(overview);

  const creds = await loadCredentials();
  if (!creds) return;
  try {
    const { skills } = await listSkills(creds);
    if (skills.length === 0) return;
    console.log("\nFetchable skills (orx skill <path>):");
    for (const s of skills) console.log(`  ${s.path}`);
  } catch {
    // API unreachable — the bundled overview is enough on its own.
  }
}
