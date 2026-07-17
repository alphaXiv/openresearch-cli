---
name: orx-evidence
description: "Analyze results: run logs, `orx search-logs`, text artifacts, W&B charts (`orx chart wandb`), and the `orx query` evidence DB."
---

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
- **For training metrics, check W&B first.** If the run has a linked W&B run
  (`orx wandb <runId>`), `orx chart wandb` is usually a better metrics read than
  grepping the log (complete history, exact stats, visible trajectory). Logs
  remain the right tool for debugging — tracebacks, OOMs, setup failures — and a
  fine metrics fallback when W&B isn't linked or doesn't have the key you need.

## Run artifacts — `orx artifacts` / `orx artifact`

Beyond the terminal log, a run uploads **text artifacts** (eval outputs, reports,
generated files).

**Where artifacts come from — you control this.** Every run has a
`.openresearch/artifacts/` directory **at the root of the repo** (the run command
executes with its working dir set to the repo root, so `.openresearch/artifacts/`
is a plain relative path from there — the same tree you cloned and edited in step
4; don't hardcode an absolute `$HOME`-based path). **Anything the run writes there
is synced to cloud storage (~every 10s, plus a final flush when the run ends) and
becomes readable later via `orx artifacts` / `orx artifact`.** Artifacts aren't
magic — they're whatever your experiment code chose to drop in that directory.
`EVAL.md` is just the conventional one (the run command writes it there); the same
mechanism is yours to use for anything you'll want to examine after the fact:
rollout transcripts, per-sample eval breakdowns, generated text, prompt/response
dumps, plots' underlying data, summary tables. When you implement a node's change
(step 4 of the loop), have the code save these to `.openresearch/artifacts/` —
that's how you turn a run into inspectable evidence instead of a one-shot log.

- **Keep things you'll re-read as text.** The CLI read commands surface **text
  artifacts** (JSON, JSONL, CSV, logs, markdown). Binary blobs — checkpoints, model
  weights, `.npy`, images — still persist to storage, but you won't be able to read
  them back through `orx artifact`, so dump a text-readable companion (e.g. a JSONL
  of rollouts alongside the checkpoint) for anything you intend to analyze via CLI.

```sh
orx artifacts <runId>               # discover what a run uploaded (KEY + SIZE table)
orx artifact <runId> <key>          # read one artifact (tail by default)
orx artifact <runId> <key> --head --bytes 200000   # from the start, raise the cap
```

- Start with `orx artifacts` to list keys, then `orx artifact <runId> <key>` to
  read one. Reading an artifact also **caches it for `orx query`** so you can grep
  artifact text via SQL.
- `orx artifact` content goes to **stdout**; byte-range and truncation metadata
  go to **stderr**.
- For the run's **code diff**, use local git — see the `orx-git` skill.

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

- `wandb` is a required first positional (the chart kind; only `wandb` is supported today).
- **`--metric`** is one W&B history key (e.g. `train/loss`). List available keys
  first via `orx query <projectId> "select distinct key from wandb_history_keys"`,
  or find linked W&B runs with `orx wandb <runId>`.
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
  command reports that and exits non-zero. Run `orx env <projectId>` first to
  confirm the key is present (it lists names only, never values).

## `orx query` — the evidence DB

The query runs against a **DuckDB "evidence" schema**, which is NOT the same
shape as the REST objects returned by `orx experiments` / `orx runs`. Don't
guess column names from what the other commands display — write queries against
the exact columns below.

The two tables you'll hit first, with their **full** column lists:

```
experiments(id, project_id, parent_experiment_id, slug, title, description,
            analysis, run_command, sandbox_id, updated_at)
runs(id, experiment_id, command, status, commit_sha, log_key, sandbox_id,
     result_markdown, updated_at)
```

The guesses that look right but aren't:

- **Experiments have no `status` — anywhere.** Status is a *run* property
  (`runs.status`). To get "the experiment's status", join its runs:
  ```sh
  orx query <projectId> "select e.title, r.status, r.updated_at from experiments e left join runs r on r.experiment_id = e.id order by e.title, r.updated_at desc"
  ```
- The parent column is **`parent_experiment_id`**, not `parent_id`.
- There is **no `branch` column** — the git branch is derived from the slug
  (`orx/<slug>`).

There is also a unified **`entities` view** (projects, experiments, runs, and
sandboxes as one table) — handy for tree/graph questions:

```
entities(id, type, entity_id, entity_type, project_id, parent_id, parent_type,
         parent_entity_id, parent_entity_type, title, name, slug, status,
         description, analysis, run_command, updated_at)
```

Caveat: its `status` column is populated **only for run and sandbox rows** —
it's NULL for experiments (see above), so don't read it off experiment rows.
For an experiment row, `parent_id` is the parent experiment (or the project,
for the baseline).

For any table not listed here, discover the real schema before writing queries:

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

These are the same skill docs the OpenResearch assistant reads.
