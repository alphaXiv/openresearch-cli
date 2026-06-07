import { type Experiment, listExperiments } from "../client.ts";
import { requireCredentials } from "../config.ts";

/** Lists a project's experiments as an indented tree (by parentExperimentId). */
export async function experiments(projectId: string | undefined): Promise<void> {
  if (!projectId) {
    console.error("Usage: orx experiments <projectId>");
    process.exit(1);
  }
  const creds = await requireCredentials();
  const { experiments } = await listExperiments(creds, projectId);

  if (experiments.length === 0) {
    console.log("No experiments in this project.");
    return;
  }

  // Group children by parent so we can walk the tree from the roots down.
  const childrenOf = new Map<string | null, Experiment[]>();
  for (const exp of experiments) {
    const key = exp.parentExperimentId;
    const list = childrenOf.get(key) ?? [];
    list.push(exp);
    childrenOf.set(key, list);
  }

  // A "root" is anything whose parent isn't present in this project's set
  // (covers both true roots and children whose parent was filtered out).
  const ids = new Set(experiments.map((e) => e.id));
  const roots = experiments.filter(
    (e) => e.parentExperimentId === null || !ids.has(e.parentExperimentId),
  );

  const printNode = (exp: Experiment, depth: number): void => {
    const indent = "  ".repeat(depth);
    console.log(`${indent}▸ ${exp.title}  (${exp.status})`);
    for (const child of childrenOf.get(exp.id) ?? []) printNode(child, depth + 1);
  };

  for (const root of roots) printNode(root, 0);
}
