#!/usr/bin/env node
import { parseArgs } from "node:util";
import { artifact } from "./commands/artifact.ts";
import { cat } from "./commands/cat.ts";
import { chart } from "./commands/chart.ts";
import { createExperiment } from "./commands/create-experiment.ts";
import { dev } from "./commands/dev.ts";
import { experiments } from "./commands/experiments.ts";
import { fsCommand } from "./commands/fs.ts";
import { login } from "./commands/login.ts";
import { logout } from "./commands/logout.ts";
import { logs } from "./commands/logs.ts";
import { projects } from "./commands/projects.ts";
import { query } from "./commands/query.ts";
import { runs } from "./commands/runs.ts";
import { search } from "./commands/search.ts";
import { searchLogsCommand } from "./commands/search-logs.ts";
import { skill } from "./commands/skill.ts";
import { tree } from "./commands/tree.ts";

const USAGE = `OpenResearch CLI

Usage:
  orx login [--api-url <url>]            Log in via the browser and store a token
  orx logout                            Remove the stored token
  orx projects [--all]                  List your projects, grouped by organization
  orx experiments <projectId>           List a project's experiments as a tree
  orx runs <projectId> [--experiment]   List a project's runs
  orx logs <runId> [--head] [--bytes <n>] [--range <s>:<e>]
                                        Read a run's terminal log (tail by default)
  orx search-logs <projectId> "<pattern>" (--run <id> | --experiment <id>) [--max <n>]
                                        Grep run logs for a literal pattern
  orx search <expId> "<query>"          Grep an experiment's committed branch (no dev node)
  orx tree <expId> [path]               List committed files (no dev node)
  orx cat <expId> <path>                Read a committed file (no dev node)
  orx artifact <runId> <key> [--head] [--bytes <n>]
                                        Read a run's text artifact (also caches it for SQL search)
  orx query <projectId> "<sql>"         Run read-only SQL against the project's evidence
  orx chart wandb <projectId> --metric "<key>" --run <runId>[:label] [--run ...] [--smoothing <n>] [--out <dir>]
                                        Render a W&B metric across runs to a PNG; prints the file path to Read
  orx create-experiment <projectId> --title "<t>" [--parent <id> | --repo <owner/repo> [--ref <r>]]
                                        Add an experiment node (child, git-repo root, or empty root)
  orx dev open <expId>                  Provision a dev node + check out the branch for editing
  orx dev status <expId>                Show dev node state + uncommitted changes
  orx dev close <expId> [-m <msg>] [--discard]
                                        Commit+push the session as one commit, then tear down
  orx read <expId> <path>               Read a file from the dev working tree
  orx write <expId> <path>              Write a file (content on stdin)
  orx str-replace <expId> <path> <old> <new>   Replace an exact unique snippet
  orx ls <expId> [path]                 List files
  orx grep <expId> <pattern>            Search files
  orx rm <expId> <path>                 Delete a file
  orx skill [path]                      Print CLI usage for agents, or fetch a skill doc

Options:
  --api-url <url>        Override the API base URL (or set OPENRESEARCH_API_URL)
  --all                  Include archived projects (projects)
  --experiment <id>      Filter to one experiment (runs)
  --title <t>            Experiment title (create-experiment, required)
  --description <t>      Experiment description (create-experiment)
  --parent <id>          Parent experiment id → create a child (create-experiment)
  --repo <owner/repo>    GitHub repo → create a root from it (create-experiment)
  --ref <ref>            Branch/tag/commit for --repo (create-experiment)
  --metric <key>         W&B history key to plot (chart)
  --run <id[:label]>     Run to overlay; repeat for multiple runs (chart); single scope (search-logs/logs)
  --smoothing <n>        EMA smoothing 0–0.99 (chart)
  --out <dir>            Directory to save the rendered PNG (chart)
  -h, --help             Show this help
`;

async function main(): Promise<void> {
  const { values, positionals } = parseArgs({
    allowPositionals: true,
    options: {
      "api-url": { type: "string" },
      all: { type: "boolean" },
      experiment: { type: "string" },
      head: { type: "boolean" },
      bytes: { type: "string" },
      range: { type: "string" },
      run: { type: "string", multiple: true },
      max: { type: "string" },
      metric: { type: "string" },
      smoothing: { type: "string" },
      out: { type: "string" },
      title: { type: "string" },
      description: { type: "string" },
      parent: { type: "string" },
      repo: { type: "string" },
      ref: { type: "string" },
      message: { type: "string", short: "m" },
      discard: { type: "boolean" },
      help: { type: "boolean", short: "h" },
    },
  });

  const command = positionals[0];

  if (values.help || !command) {
    console.log(USAGE);
    return;
  }

  switch (command) {
    case "login":
      await login({ apiUrl: values["api-url"] });
      break;
    case "logout":
      await logout();
      break;
    case "projects":
      await projects({ all: values.all });
      break;
    case "experiments":
      await experiments(positionals[1]);
      break;
    case "runs":
      await runs(positionals[1], { experimentId: values.experiment });
      break;
    case "logs":
      await logs(positionals[1], {
        head: values.head,
        bytes: values.bytes,
        range: values.range,
      });
      break;
    case "search-logs":
      await searchLogsCommand(positionals[1], positionals[2], {
        run: values.run?.[0],
        experiment: values.experiment,
        max: values.max,
      });
      break;
    case "search":
      await search(positionals[1], positionals[2]);
      break;
    case "tree":
      await tree(positionals[1], positionals[2]);
      break;
    case "cat":
      await cat(positionals[1], positionals[2]);
      break;
    case "artifact":
      await artifact(positionals[1], positionals[2], {
        head: values.head,
        bytes: values.bytes,
      });
      break;
    case "query":
      await query(positionals[1], positionals[2]);
      break;
    case "chart":
      await chart(positionals[1], positionals[2], {
        metric: values.metric,
        run: values.run,
        smoothing: values.smoothing,
        out: values.out,
      });
      break;
    case "create-experiment":
      await createExperiment(positionals[1], {
        title: values.title,
        description: values.description,
        parent: values.parent,
        repo: values.repo,
        ref: values.ref,
      });
      break;
    case "dev":
      await dev(positionals[1], positionals[2], {
        message: values.message,
        discard: values.discard,
      });
      break;
    case "read":
    case "write":
    case "str-replace":
    case "ls":
    case "grep":
    case "rm":
      await fsCommand(command, positionals[1], positionals.slice(2));
      break;
    case "skill":
      await skill(positionals[1]);
      break;
    default:
      console.error(`Unknown command: ${command}\n`);
      console.log(USAGE);
      process.exit(1);
  }
}

main().catch((err: unknown) => {
  console.error(err instanceof Error ? err.message : String(err));
  process.exit(1);
});
