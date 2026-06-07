---
name: openresearch-cli
description: Use the `orx` CLI to inspect OpenResearch projects from a terminal — list projects, walk an experiment tree, list runs, and run read-only SQL against a project's evidence. Read this before driving `orx` programmatically.
---

# OpenResearch CLI (`orx`)

`orx` is a thin command-line client over the OpenResearch API. It authenticates
with a personal access token and exposes read-only views of a project's
experiment tree, runs, and evidence database. Use it when you need to inspect
project state from a shell instead of the web UI.

## Setup

```sh
orx login          # opens a browser, stores a token at ~/.config/openresearch/credentials.json
```

- The API base URL resolves from `--api-url` → `OPENRESEARCH_API_URL` → a built-in
  default. Set `OPENRESEARCH_API_URL` for non-local use.
- Every other command needs a token; if you see `Not logged in`, run `orx login`.

## Commands

| Command | What it does |
|---|---|
| `orx projects [--all]` | List your projects (id + name), grouped by org. `--all` includes archived. **Project ids come from here** — copy one into the commands below. |
| `orx experiments <projectId>` | Print the project's experiments as an indented tree (nested by parent). |
| `orx runs <projectId> [--experiment <id>]` | List runs as a table (status, experiment, commit, updated). `--experiment` filters to one experiment. |
| `orx logs <runId> [--head] [--bytes <n>] [--range <s>:<e>]` | Read a run's terminal log. See below. |
| `orx search-logs <projectId> "<pattern>" (--run <id> \| --experiment <id>)` | Grep run logs for a literal pattern. See below. |
| `orx query <projectId> "<sql>"` | Run **one read-only DuckDB SQL statement** against the project's evidence schema. |
| `orx chart wandb <projectId> --metric "<key>" --run <runId>[:label] ...` | Render a W&B metric across runs to a PNG line chart. See below. |
| `orx create-experiment <projectId> --title "<t>" [...]` | Add an experiment node (write). See below. |
| `orx dev open/close/status <expId>` + `orx read/write/str-replace/ls/grep/rm <expId>` | Edit a node's files in a dev session. See below. |
| `orx skill [path]` | Print this overview (no args), or fetch a deeper skill/reference doc by path. |

Every data command takes a **project id** (never an experiment id) as its scope.
Get ids from `orx projects`.

## `orx create-experiment` — the one write command

Adds a node to the experiment tree. `--title` is always required. The node shape
is chosen by flags:

```sh
# Child node, branched off an existing experiment:
orx create-experiment <projectId> --title "Larger batch size" --parent <experimentId>

# Root node imported from a GitHub repo (owner/repo, via the org's GitHub App):
orx create-experiment <projectId> --title "Baseline" --repo lino-levan/sortbench --ref main

# Empty root node:
orx create-experiment <projectId> --title "Scratch baseline"
```

- `--parent` and `--repo` are mutually exclusive (`--parent` ⇒ child, `--repo` ⇒
  root-from-repo, neither ⇒ empty root).
- `--repo` takes a GitHub `owner/repo` that is reachable through the org's GitHub
  App installation — it is imported as a tarball, **not** an arbitrary
  `git clone` URL. `--ref` (branch/tag/commit) only applies with `--repo`.
- `--description` is optional in all three shapes.

## Editing a node's files — `orx dev` sessions

To change files in an experiment, open a short-lived **dev session**: a CPU node
with the branch checked out. You edit the *live working tree* (no commits per
edit), then `dev close` makes **one commit** and tears the node down.

```sh
orx dev open <expId>                         # provisions a node (~30s), checks out the branch
orx ls   <expId> src                         # explore
orx read <expId> src/train.py
orx str-replace <expId> src/train.py "lr=1e-3" "lr=3e-4"
cat new_config.yaml | orx write <expId> config.yaml   # write content from stdin
orx grep <expId> "batch_size"
orx dev status <expId>                        # state + uncommitted changes
orx dev close <expId> -m "tune lr"            # ONE commit + push, then teardown
#   orx dev close <expId> --discard           # tear down without committing
```

Rules and notes:
- **Always `orx dev open` first.** The edit verbs (`read`/`write`/`str-replace`/`ls`/`grep`/`rm`)
  fail with a clear error if no dev node is open.
- Edits do **not** commit individually — they accumulate in the working tree.
  Only `dev close` commits (one commit for the whole session).
- `write` reads the file content from **stdin**. `str-replace` needs the
  `old_string` to appear exactly once.
- **Close when done.** If you forget, the node auto-tears-down after ~30 min idle
  (hard cap ~4 h) — but `dev close` is the intended path and avoids wasted compute.
- All paths are relative to the experiment workdir; `..` and `.git` are blocked.

## Reading & searching run logs — `orx logs` / `orx search-logs`

A run's terminal output (the PTY stream) is captured live while it runs and
persisted afterwards. These two commands read and grep it the same way the
OpenResearch assistant's "Read run log" / "Search run log" tools do — byte-range
reads against the persisted log, the live buffer for an in-flight run.

```sh
orx logs <runId>                    # tail (the end — usually what you want)
orx logs <runId> --head             # read from the start instead
orx logs <runId> --bytes 200000     # raise the byte cap (default 64 KB, max 1 MB)
orx logs <runId> --range 4096:8192  # exact byte window [start, end)
```

- The log goes to **stdout** (pipe/redirect-friendly); a `[source] bytes a–b of N`
  status line goes to **stderr**, noting if content was truncated above/below.
- `<runId>` comes from `orx runs <projectId>` (the run id, not the experiment id).

```sh
# Grep a single run, or every run in an experiment:
orx search-logs <projectId> "CUDA out of memory" --run <runId>
orx search-logs <projectId> "Traceback"          --experiment <experimentId>
orx search-logs <projectId> "loss=nan" --experiment <id> --max 5000
```

- Search is **literal and case-sensitive**. One of `--run` / `--experiment` is required.
- Each hit prints as `<run8>:<line>: <text>  ← <startByte>:<endByte>`. Feed those
  byte offsets straight into `orx logs <runId> --range <start>:<end>` to pull the
  surrounding context. Results are capped (raise with `--max`).

## Charting W&B metrics — `orx chart wandb`

Renders a single W&B history metric across one or more linked runs as a PNG line
chart — the same renderer the OpenResearch assistant uses. The server fetches the
W&B history, draws the chart, and the CLI **downloads the PNG to a local file and
prints its path**. Because the output is an image, the intended flow is: run the
command, then **`Read` the printed PNG path to view the chart** with your vision.

```sh
# One metric, two runs overlaid (label after a colon, optional):
orx chart wandb <projectId> --metric "train/loss" \
  --run <runId>:experiment --run <baselineRunId>:baseline

orx chart wandb <projectId> --metric "train/reward" --run <runId> --smoothing 0.9
orx chart wandb <projectId> --metric "val/acc" --run <runId> --out ./charts
```

- **`--metric`** is one W&B history key (e.g. `train/loss`). List available keys
  first via `orx query <projectId> "select distinct key from wandb_history_keys"`.
- **`--run`** is repeatable — pass every run you want on the chart in one call
  (up to 6). Append `:label` to set the legend label (defaults to the W&B run id).
  The run id comes from `orx runs <projectId>` (the run id, not the experiment id).
- **`--smoothing`** is an EMA factor `0`–`0.99` (default `0.6`). If a chart looks
  too noisy to read, re-run with a higher value (e.g. `0.9`) — don't switch metrics.
- **`--out`** sets the output directory (default `~/.cache/openresearch/charts/`).
- Per-run summary stats (`n`, `min`, `max`, `last`) print to **stdout** alongside
  the file path, so you can cite exact numbers without opening the image. Runs
  that produced no data are listed under `Skipped:`.
- Requires `WANDB_API_KEY` set in the project or org env vars; otherwise the
  command reports that and exits non-zero.

## `orx query` — important

The query runs against a **DuckDB "evidence" schema**, which is NOT the same
shape as the REST objects returned by `orx experiments` / `orx runs`. For
example, the evidence `experiments` table has `title`, `slug`, `description`,
`analysis`, `sandbox_id` — but **no `status` column**. Don't assume column names.

Discover the real schema before writing queries:

```sh
orx query <projectId> "select table_name, column_name from information_schema.columns order by 1, 2"
```

The full schema, table-by-table guidance, and worked examples live in the
canonical project-query skill — fetch it before doing anything non-trivial:

```sh
orx skill project-query                              # the schema + workflow guide
orx skill project-query/references/runs-and-results  # runs, metrics, results
orx skill project-query/references/run-diffs          # code diffs per run
orx skill project-query/references/text-evidence      # logs, artifacts, files
orx skill project-query/references/project-overview    # high-level project shape
```

These are the same skill docs the OpenResearch assistant reads. Run
`orx skill` with no path to re-print this overview; the list of fetchable skills
is also shown there when the API is reachable.

## Typical workflow

```sh
orx projects                     # find the project id
orx experiments <projectId>      # see the tree
orx skill project-query          # learn the evidence schema
orx query <projectId> "select title from experiments limit 10"
```
