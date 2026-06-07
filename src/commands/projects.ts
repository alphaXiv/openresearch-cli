import { listOrgs, listProjects } from "../client.ts";
import { requireCredentials } from "../config.ts";

interface ProjectsOptions {
  all?: boolean;
}

/** Lists project names, grouped by organization. */
export async function projects(options: ProjectsOptions): Promise<void> {
  const creds = await requireCredentials();
  const { orgs } = await listOrgs(creds);

  if (orgs.length === 0) {
    console.log("No organizations found for this account.");
    return;
  }

  for (const org of orgs) {
    const { projects } = await listProjects(creds, org.id);
    const visible = options.all ? projects : projects.filter((p) => !p.archived);

    console.log(`\n${org.name}`);
    if (visible.length === 0) {
      console.log("  (no projects)");
      continue;
    }
    // Id first (fixed-width) so names line up and ids are easy to copy into
    // `orx experiments/runs/query <projectId>`.
    const idWidth = Math.max(...visible.map((p) => p.id.length));
    for (const project of visible) {
      const tag = project.archived ? " (archived)" : "";
      console.log(`  ${project.id.padEnd(idWidth)}  ${project.name}${tag}`);
    }
  }
}
