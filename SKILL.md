---
name: openresearch-cli
description: Use the `orx` CLI to drive OpenResearch projects from a terminal — browse the experiment tree, runs, logs, artifacts, and the evidence DB; create experiments; launch, wait on, and cancel runs on GPU compute; and chart W&B metrics. Each experiment is a git branch in a local cache-dir clone — reading, diffing, and editing code all happen there with plain git. Read this before driving `orx` programmatically.
---

# OpenResearch CLI (`orx`)

`orx` is a command-line client over the OpenResearch API. It authenticates with a
personal access token and exposes both **read views** of a project (experiment
tree, runs, logs, artifacts, evidence database) and **write actions**
(create experiments, launch/cancel runs on GPU compute). Code is the one thing
`orx` does not serve: every experiment is a git branch on the project's GitHub
repo, and the **local clone in `~/.cache/openresearch/repos/<owner>/<repo>` is
the standard way to read, diff, and edit it** (see the `orx-git` module). Use
`orx` when you need to inspect or drive project state from a shell instead of the
web UI.

This overview is deliberately short: it carries the cardinal rules and a command
quick-reference, then points at focused **modules** for everything else. Load a
module with `orx skill <name>` (the live index is printed at the end of `orx
skill` output).

## Cardinal rules — read before doing anything else

These four govern everything below. Breaking any one silently invalidates your
results — they are not style preferences. The `orx-experiment-tree` module
expands on the why; these are the non-negotiables.

1. **Never edit a baseline (root) once it holds real code.** A root is the
   control its variants are measured against. Projects start with an empty
   tree — **you create the baseline** (the first `orx create-experiment`, no
   `--parent`) and, on a blank repo, seed it with starting code before its first
   run (see the `orx-create` module). That setup window is the one
   exception, like rule 2's single legitimate `--set`. From the moment a root
   holds real code this rule is absolute: to try an idea, **branch a child**
   and edit the child. Editing a root moves the goalposts and destroys every
   comparison under it.
2. **The run command *and* the environment are a fixed contract — identical on
   every node.** A child inherits its parent's run command verbatim; leave it
   alone. Do **not** give nodes different start commands, and do **not** vary
   behavior through environment variables or env-prefixed commands
   (`LR=3e-4 python …`). The *only* thing that may differ between nodes is the
   **committed code/config** on the node's git branch. `orx exp cmd --set` is
   legitimate exactly once: to set the baseline's command when it has none.
3. **Vary code, not knobs-in-the-command.** Encode hyperparameters in the
   code/config files and branch a child per variant — never sweep them by editing
   the run command or passing env vars. Every node runs the *same* command over
   *different code*, so their `EVAL.md` outputs stay comparable.
4. **Grow the tree downward, not sideways.** Fan a little *within* a round (the
   options of one decision), then **descend onto that round's winner** for the
   next round. A root with a long row of direct children and no grandchildren is
   the failure mode. See "Shape the tree" in the `orx-experiment-tree` module.

If you're ever tempted to change the command, pass an env var, or pile another
node onto the root instead of branching a child, editing its branch, and
descending — stop. That's the anti-pattern, not a shortcut.

## Setup

```sh
orx login          # opens a browser, stores a token at ~/.config/openresearch/credentials.json
orx logout         # remove the stored token
```

- The API base URL resolves from `--api-url` → `OPENRESEARCH_API_URL` → a built-in
  default. Set `OPENRESEARCH_API_URL` for non-local use.
- Every command except `login`, `lit`, and `paper` needs a token; if you see `Not logged in`, run `orx login`. (`lit` and `paper` hit alphaXiv's public hosts and work without one.)

## Command quick-reference

Project-scoped commands take a **project id**; experiment-scoped commands take an
**experiment id**; run-scoped commands take a **run id**. Don't mix them — get
ids from `orx projects`, `orx experiments`, and `orx runs` respectively. Each
group below has a module (`orx skill <name>`) with the full flags and rules.

### Auth
| Command | What it does |
|---|---|
| `orx login [--api-url <url>]` | Open a browser, do loopback OAuth, store a token. |
| `orx logout` | Remove the stored token. |

### Discover (project- and experiment-scoped)
| Command | What it does |
|---|---|
| `orx projects [--all] [--json]` | List your projects (id + name + GitHub `owner/repo`), grouped by org. `--all` includes archived; `--json` emits a flat array (incl. each project's `paperId`) for scripts. **Project ids, org ids, and the repo to clone come from here.** |
| `orx explore [--json]` | List the **public** project directory (id + name + repo) — projects anyone can view. Drill in with `orx project view` / `orx experiments` / `orx runs`. |
| `orx project view <projectId>` | Overview of one project: details, its experiment tree, and its reports. Works on any public project or any private one in your orgs. |
| `orx experiments <projectId>` | Print the project's experiments as an indented tree. **Experiment ids come from here.** |
| `orx runs <projectId> [--experiment <id>]` | List runs as a table, newest first. **Run ids come from here.** |
| `orx env <projectId>` | List the **names** of the env vars a run will see (merged org + project + per-user), each tagged with its source. **Names only — values never returned.** |

### Run evidence (run-scoped) — module `orx-evidence`
| Command | What it does |
|---|---|
| `orx logs <runId> [--head] [--bytes <n>] [--range <s>:<e>]` | Read a run's terminal log. |
| `orx search-logs <projectId> "<pattern>" (--run <id> \| --experiment <id>) [--max <n>]` | Grep run logs for a literal pattern. |
| `orx artifacts <runId>` / `orx artifact <runId> <key> [--head] [--bytes <n>]` | List / read a run's text artifacts. |
| `orx wandb <runId>` | List the W&B runs linked to a run. |
| `orx chart wandb <projectId> --metric "<key>" --run <runId>[:label] ...` | Render a W&B metric across runs to a PNG. |
| `orx query <projectId> "<sql>"` | Run one read-only DuckDB SQL statement against the evidence schema. |

### Create and run experiments (write) — modules `orx-create`, `orx-compute`, `orx-git`
| Command | What it does |
|---|---|
| `orx create-project <orgId> --name "<n>" [--repo <owner/repo>]` | Create a project bound to a GitHub repo (or a fresh blank repo). |
| `orx project edit <projectId> [--name "<n>"] [--description "<text>" \| --description-stdin]` | Edit a project's name and/or description (pass at least one); `--description-stdin` overwrites the description from stdin (long markdown). |
| `orx create-experiment <projectId> --title "<t>" [...]` | Add an experiment node; prints its git branch. |
| `orx compute [--gpu <id>] [--count <n>] [--provider <name>] \| --cpu]` | List the GPU/CPU compute catalog. |
| `orx instance create <orgId> (--gpu <id> … \| --cpu <flavor> …)` | Spin up a standalone instance in an org. |
| `orx exp status/cmd/run/cancel/wait <expId>` | Inspect, run, cancel, and wait on a single experiment node. |
| `orx exp desc <expId> [--set "<text>" \| --stdin]` | Read or overwrite the experiment's description. |
| `orx report upload/list/show/download <projectId> …` | Publish and read project reports (module `orx-reports`). |

To **read or edit** a node's code — including diffing what a run changed — use
plain git in the cache-dir clone; there is no `orx` code command. See the
`orx-git` module.

### Literature & papers — alphaXiv (no login required) — module `orx-lit`
| Command | What it does |
|---|---|
| `orx lit "<query>" [--limit <n>] [--json]` | Full-text search alphaXiv's paper corpus. |
| `orx paper <id\|url> [--full]` | Fetch a paper's machine-readable report (or full text with `--full`). |

### Meta
| Command | What it does |
|---|---|
| `orx skill [name]` | Print this overview + the live module index (no args), or print one module / fetch a deeper reference doc by name. |

## Modules

The detail lives in focused modules — load one with `orx skill <name>` (the live
list, with one-line descriptions, is printed at the end of `orx skill` output):

- **orx-experiment-tree** — the experiment-tree model, the auto-research loop, and `orx exp desc`.
- **orx-create** — create a project, seed an empty baseline, add experiment nodes.
- **orx-compute** / **orx-compute-k8s** — launch runs on compute; the k8s manifest contract.
- **orx-git** — read, edit, and diff a node's code with plain git.
- **orx-evidence** — logs, search-logs, artifacts, W&B charts, and the `orx query` evidence DB.
- **orx-reports** — write and publish research reports.
- **orx-lit** — literature search and paper content.

Deeper API-served references (the project-query schema and worked examples, the
report writing guide) are fetchable too — `orx skill` lists them at the end when
the API is reachable.

## Typical workflow

Orienting in a project (read-only discovery):

```sh
orx projects                     # find the project id
orx experiments <projectId>      # see the tree, pick an experiment id
orx skill experiment-tree        # the model + the auto-research loop
orx runs <projectId>             # find a run id
orx logs <runId>                 # read its output
```

To actually **drive** a project toward a goal — branch children off the baseline,
edit each child's code on its git branch, and keep the GPU budget saturated —
follow the auto-research loop in the `orx-experiment-tree` module.
