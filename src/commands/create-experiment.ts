import {
  createChildExperiment,
  createEmptyBaseline,
  type Experiment,
  importBaseline,
} from "../client.ts";
import { requireCredentials } from "../config.ts";

interface CreateOptions {
  title?: string;
  description?: string;
  parent?: string;
  repo?: string;
  ref?: string;
}

const USAGE =
  'Usage: orx create-experiment <projectId> --title "<title>" [--parent <experimentId>] [--repo <owner/repo> [--ref <ref>]] [--description "<text>"]';

/**
 * Creates an experiment node. Three shapes, picked by flags:
 *   --parent <id>   → child experiment branched off that parent
 *   --repo a/b      → root experiment imported from a GitHub repo
 *   (neither)       → empty root experiment
 * A title is always required.
 */
export async function createExperiment(
  projectId: string | undefined,
  options: CreateOptions,
): Promise<void> {
  if (!projectId || !options.title) {
    console.error(USAGE);
    process.exit(1);
  }
  if (options.parent && options.repo) {
    console.error("Choose one of --parent or --repo, not both.");
    console.error("(--parent makes a child node; --repo makes a root node from a git repo.)");
    process.exit(1);
  }
  if (options.ref && !options.repo) {
    console.error("--ref only applies together with --repo.");
    process.exit(1);
  }

  const creds = await requireCredentials();
  const { title, description } = options;

  let experiment: Experiment;
  let kind: string;
  if (options.parent) {
    ({ experiment } = await createChildExperiment(creds, projectId, {
      title,
      description,
      parentExperimentId: options.parent,
    }));
    kind = "child";
  } else if (options.repo) {
    // The repo must be a GitHub repo ("owner/repo") reachable through the org's
    // GitHub App installation — it's imported via tarball, not an arbitrary
    // `git clone` URL. `patch` is required by the endpoint; we send null.
    ({ experiment } = await importBaseline(creds, projectId, {
      repoFullName: options.repo,
      ref: options.ref ?? "",
      patch: null,
      title,
      description,
    }));
    kind = "root (from " + options.repo + ")";
  } else {
    ({ experiment } = await createEmptyBaseline(creds, projectId, { title, description }));
    kind = "root (empty)";
  }

  console.log(`✓ Created ${kind} experiment`);
  console.log(`  id:    ${experiment.id}`);
  console.log(`  title: ${experiment.title}`);
  console.log(`  slug:  ${experiment.slug}`);
}
