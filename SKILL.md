---
name: openresearch-cli
description: Use the `orx` CLI to drive OpenResearch projects from a terminal — browse the experiment tree, runs, logs, artifacts, code diffs, and the evidence DB; create experiments; launch, wait on, and cancel runs on GPU compute; edit a node's files in a dev session; and chart W&B metrics. Read this before driving `orx` programmatically.
---

# OpenResearch CLI (`orx`)

`orx` is a command-line client over the OpenResearch API. It authenticates with a
personal access token and exposes both **read views** of a project (experiment
tree, runs, logs, artifacts, code diffs, evidence database) and **write actions**
(create experiments, launch/cancel runs on GPU compute, edit a node's files). Use
it when you need to inspect or drive project state from a shell instead of the
web UI.

## Setup

```sh
orx login          # opens a browser, stores a token at ~/.config/openresearch/credentials.json
orx logout         # remove the stored token
```

- The API base URL resolves from `--api-url` → `OPENRESEARCH_API_URL` → a built-in
  default. Set `OPENRESEARCH_API_URL` for non-local use.
- Every command except `login`, `lit`, and `paper` needs a token; if you see `Not logged in`, run `orx login`. (`lit` and `paper` hit alphaXiv's public hosts and work without one.)

## Commands

### Auth
| Command | What it does |
|---|---|
| `orx login [--api-url <url>]` | Open a browser, do loopback OAuth, store a token. |
| `orx logout` | Remove the stored token. |

### Discover (project- and experiment-scoped)
| Command | What it does |
|---|---|
| `orx projects [--all]` | List your projects (id + name), grouped by org. `--all` includes archived. **Project ids come from here.** |
| `orx experiments <projectId>` | Print the project's experiments as an indented tree (nested by parent). **Experiment ids come from here.** |
| `orx runs <projectId> [--experiment <id>]` | List runs as a table (status, experiment, commit, updated), newest first. `--experiment` filters to one experiment. **Run ids come from here.** |

### Run evidence (run-scoped)
| Command | What it does |
|---|---|
| `orx logs <runId> [--head] [--bytes <n>] [--range <s>:<e>]` | Read a run's terminal log. See below. |
| `orx search-logs <projectId> "<pattern>" (--run <id> \| --experiment <id>) [--max <n>]` | Grep run logs for a literal pattern. See below. |
| `orx artifacts <runId>` | List the text artifacts a run produced (key + size). |
| `orx artifact <runId> <key> [--head] [--bytes <n>]` | Read a run's text artifact (tail by default). Also caches it for `orx query` SQL search. |
| `orx wandb <runId>` | List the W&B runs linked to a run (with dashboard URLs). |
| `orx diff <runId>` | Print a run's cumulative code diff vs. its parent experiment's branch. |
| `orx chart wandb <projectId> --metric "<key>" --run <runId>[:label] ...` | Render a W&B metric across runs to a PNG line chart. See below. |
| `orx query <projectId> "<sql>"` | Run **one read-only DuckDB SQL statement** against the project's evidence schema. See below. |

### Committed code — no dev node needed (experiment-scoped)
| Command | What it does |
|---|---|
| `orx tree <expId> [path]` | List committed files in the experiment's branch under an optional path. |
| `orx cat <expId> <path>` | Print a committed file from the experiment's branch to stdout. |
| `orx search <expId> "<query>"` | Grep the committed branch for a case-insensitive substring. |

### Create, run, and edit experiments (write)
| Command | What it does |
|---|---|
| `orx create-experiment <projectId> --title "<t>" [...]` | Add an experiment node (the one project-level write command). See below. |
| `orx compute [--gpu <id>] [--count <n>]` | List the GPU compute catalog (price-sorted). See below. |
| `orx exp status/cmd/run/cancel/wait <expId>` | Inspect, run, cancel, and wait on a single experiment node. See below. |
| `orx exp desc <expId> [--set "<text>" \| --stdin]` | Read or overwrite the experiment's description (free-form notes). See below. |
| `orx dev open/close/status <expId>` + `orx read/write/str-replace/ls/grep/rm <expId>` | Edit a node's files in a dev session. See below. |

### Literature & papers — alphaXiv (no login required)
| Command | What it does |
|---|---|
| `orx lit "<query>" [--limit <n>] [--json]` | Full-text search alphaXiv's paper corpus; returns ranked hits (id, title, date, votes, abstract). See below. |
| `orx paper <id\|url>` | Fetch a paper's **machine-readable report** (structured LLM-oriented analysis). See below. |
| `orx paper <id\|url> --full` | Fetch the paper's **full extracted text** instead — fallback when the report lacks a specific detail. |

### Meta
| Command | What it does |
|---|---|
| `orx skill [path]` | Print this overview (no args), or fetch a deeper skill/reference doc by path. |

Project-scoped commands take a **project id**; experiment-scoped commands take an
**experiment id**; run-scoped commands take a **run id**. Don't mix them — get
ids from `orx projects`, `orx experiments`, and `orx runs` respectively.

## The experiment-tree model — read this before driving a project

A project is a **tree of experiment nodes**. The root (**baseline**) holds the
starting code and a **run command** — the single shell command that trains or
evaluates the node and writes an `EVAL.md` with its results. Every other node is a
**child** branched off a parent, inheriting its code and its run command.

Two rules drive everything you do here:

1. **The run command is a fixed contract, not a knob.** A child **inherits its
   parent's run command** (`create-experiment --parent` copies it verbatim). You
   compare nodes by running *the same command* over *different code*, then diffing
   their `EVAL.md` outputs. Change the command per node and the results stop being
   comparable. So: set the run command **once on the baseline**, then leave it
   alone — vary the **code/config**, never the command. Do **not** encode
   hyperparameters as CLI args in the run command and sweep them by editing the
   command; encode them in the code/config files and branch a child per variant.

2. **Never edit the baseline.** The root is your control — the anchor every variant
   is measured against. To try an idea, **branch a child** off it and edit the
   child. Editing the root moves the goalposts and destroys the comparison.

**Climb the tree — don't fan out from the root.** There are two reasons to branch,
and they want opposite shapes:

- **Comparing orthogonal knobs head-to-head** (LR vs. width vs. init — each measured
  against the *same* control): branch each off the **baseline**. Wide is correct here.
- **Composing or refining** (cosine LR won → now test width *on top of* cosine LR so
  the gains stack): branch off the **best confirmed node so far**, not the baseline.
  Deep is correct here.

A baseline with 10+ direct children and **no grandchildren** is the failure mode: you're
sweeping, not climbing. Every result is being measured against the *start* instead of
building on the last win, so improvements never accumulate. After each round produces a
winner, the focal parent should move **down** the tree — the next round's children branch
off that winner. A healthy tree gets *deeper* as the research progresses, not just wider.

### The auto-research loop

To drive a project toward a goal (e.g. "best convergence for d=8") under a fixed
GPU budget, this is the intended flow — do **not** edit the baseline or rewrite the
run command:

1. **Read the baseline's code.** `orx tree <baseId>`, `orx cat <baseId> <path>`,
   `orx search <baseId> "<sym>"`. See its run command with `orx exp cmd <baseId>`
   and find where the knobs live (config files, hyperparameters, model defs).
2. **Form hypotheses** — concrete, independent ideas (an LR schedule, a width
   change, an init scheme, …), each a single change you can make and measure.
3. **Create one child per idea — and pick its parent deliberately.** The **title**
   is the idea, the **description** is the concrete change the dev session will make.
   The parent is *not* always the baseline: branch off the baseline only when you're
   isolating an orthogonal variable for a clean head-to-head against the control. Once
   an earlier round has produced a **confirmed winner**, branch this round's children
   off **that winner** instead, so the new change stacks on top of the gain rather than
   resetting to the start (see "Climb the tree" above).
   ```sh
   # Round 1: orthogonal knobs, each off the baseline (wide — fair head-to-head):
   orx create-experiment <projectId> --parent <baseId> \
     --title "Cosine LR + warmup" \
     --description "Switch the constant LR in config.yaml to cosine decay with 500-step warmup; leave everything else."

   # Round 2: cosine LR won → stack the next idea ON it (deep — compose the gains):
   orx create-experiment <projectId> --parent <cosineWinnerId> \
     --title "Wider MLP on cosine LR" \
     --description "On top of the cosine-LR winner, widen the MLP hidden dim 1024→2048 in model.py."
   ```
   The child inherits its parent's run command automatically — you don't set it.
4. **Implement each child's change in a dev session** — edit only the files that
   idea touches, and **leave the run command alone**:
   ```sh
   orx dev open <childId>
   orx str-replace <childId> config.yaml "schedule: constant" "schedule: cosine"
   orx dev close <childId> -m "cosine LR + warmup"
   ```
5. **Launch up to your GPU budget** — one run per ready child, in parallel:
   ```sh
   orx exp run <childId> --gpu H100 --count 1
   ```
6. **Keep the budget saturated — drive a per-completion loop, not a wait-for-all
   barrier.** With a cap of N concurrent runs, you want control back the moment
   *any one* run finishes so you can analyze the state of experiments and either refill its slot or stop if no experiment further is needed — not after the whole batch
   drains. `orx exp wait --project <projectId>` is built for exactly this: it
   returns on the **first** completion. Treat it as one **tick** of a loop, where
   *you* are the loop body:

   ```
   # after launching your runs, loop until the project is drained:
   loop:
     orx exp wait --project <projectId>   # sleeps; returns on the first completion
     orx runs <projectId>                 # SOURCE OF TRUTH: re-read all run states
     # for each run now terminal that you haven't handled yet:
     #   - read its results (step 7) and decide: launch a refill? promote it? stop?
     #   - launch the next queued child to refill the freed slot (step 5)
     # if `exp wait` printed "drained: no runs in flight"  → batch is done, break
   ```

   Three things make this robust — follow all of them:
   - **`exp wait --project` is a sleep-until-change signal, not the source of
     truth.** It only reports completions it observed *during that one call*. A
     run that finishes while you're analyzing the previous one is already terminal
     by the next call and **won't be reported**. So on every wake, re-read
     `orx runs <projectId>` and reconcile against the set of runs you've already
     handled — act on *every* newly-terminal run, not just the line `exp wait`
     printed. (This is the one time you do look at `orx runs` in a loop — as the
     reconcile after each wake, **not** as a tight poll in place of `exp wait`.)
   - **Re-issue `exp wait` each tick.** One completion → one return → you decide →
     you call it again. Don't expect a single `exp wait` to block until everything
     is done; that's the failure mode this loop avoids.
   - **Terminate on drained.** When no runs are in flight, `exp wait --project`
     returns immediately printing `drained: no runs in flight`. That — or seeing
     every run terminal in `orx runs` with no more children to launch — is your
     exit condition. Don't keep calling it into a timeout.
7. **Analyze each finish as it lands, then iterate.** Do the per-completion read
   *inside the loop above*, not deferred to the end — when a run finishes,
   **actually read its results** before deciding: `orx artifact <runId> EVAL.md`,
   `orx chart wandb …`, `orx query …`. Don't infer from status alone. Each
   completion is a decision point with three moves:
   - **Refill** — result is mediocre or inconclusive: launch the next queued child to
     keep the GPU budget saturated (step 5).
   - **Promote** — result is a clear win: this node becomes the **parent for the next
     round**. The next batch of children branch off *it*, not the baseline, so the win
     carries forward and the next ideas stack on top of it. This is the move that makes
     the tree grow deeper; skipping it is what produces a flat, sweep-only tree.
   - **Stop** — goal met, or the branch is exhausted.

   The baseline stays untouched throughout — promotion moves the *focal parent* down the
   tree, it never edits the root.

Stop when the goal is met, or after ~3 consecutive failed or regressed runs.

## `orx create-experiment` — the one project-level write command

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
- **A `--parent` child inherits the parent's run command** (and branches off its
  code). You do **not** set a run command on the child — keep it and vary the code
  via a dev session (see "the experiment-tree model" above).
- `--repo` takes a GitHub `owner/repo` that is reachable through the org's GitHub
  App installation — it is imported as a tarball, **not** an arbitrary
  `git clone` URL. `--ref` (branch/tag/commit) only applies with `--repo`.
- `--description` is optional but **strongly recommended for children**: use it to
  record the hypothesis / the concrete change this node makes. It's the node's
  free-form notes field (the same one `orx exp desc` reads/writes), and it's how
  you and the analysis tools tell sibling variants apart.

## Running an experiment — `orx exp` + `orx compute`

Each experiment node has a **run command** (the shell command that trains/evaluates
it) and is launched on **compute** you choose at run time. Compute is *not* stored
on the node — you pick a GPU (or an existing sandbox) each time you launch.

```sh
orx exp status <expId>                 # status, run command, sandbox link, latest run
orx exp cmd <expId>                    # print the current run command
orx exp cmd <baseId> --set "bash run.sh"   # set it ONCE on the baseline; children inherit it
orx compute                            # browse GPU offers (price-sorted)
orx compute --gpu H100 --count 1       # filter the catalog
orx exp run <expId> --gpu H100 --count 1 [--disk 100]     # launch on a NEW instance
orx exp run <expId> --sandbox <sandboxId>                 # launch on an EXISTING node
orx exp cancel <expId>                 # cancel the in-flight run
```

Rules and notes:
- **The run command is a fixed contract — set it once on the baseline, then leave
  it alone.** Children inherit it (see "the experiment-tree model" above). Don't
  `--set` a different command per child, and don't bake swept hyperparameters into
  it — vary the **code/config** in a child's dev session instead, so every variant
  runs the same command and their `EVAL.md`s stay comparable. The normal reason to
  touch a command is the baseline having none yet.
- **Set a run command before launching.** `orx exp run` fails with a pointer to
  `orx exp cmd --set` if the node has none.
- **Pick compute with exactly one of `--gpu` or `--sandbox`.** With `--gpu`,
  `--count` defaults to `1` and `--disk` to `100` (GB). New instances are
  **RunPod-only** — the server picks the cheapest matching RunPod offer for the
  chosen (gpu, count); browse valid gpu ids and prices with `orx compute`.
- `orx exp run` **queues** the run and returns immediately — it does not wait.
  Follow progress with `orx runs <projectId>` and `orx logs <runId>`, or block
  with `orx exp wait` (below).

## Waiting on runs — `orx exp wait`

Block until a run changes state — useful when driving a research loop and you want
to act as soon as a run finishes. Two modes, picked by argument:

```sh
orx exp wait <expId>                    # level trigger: poll this experiment's latest run
                                        #   until it reaches a terminal state (done/failed/cancelled)
orx exp wait --project <projectId>      # edge trigger: return when the FIRST run in the
                                        #   project COMPLETES (transitions into done/failed/cancelled)
orx exp wait <expId> --interval 10 --timeout 3600   # tune polling
```

- Pass **exactly one** of `<expId>` or `--project` (not both, not neither).
- `--project` is the **budget-loop** primitive: it wakes only on a **completion**
  (a run reaching `done`/`failed`/`cancelled`) — i.e. a freed slot. Run *starts*,
  new queued runs, and `queued→running` transitions are intentionally ignored, so
  it won't wake you on non-events. It returns on the **first** completion — call
  it in a loop, one return per tick, and you (not the CLI) decide what to do with
  each freed slot. See the per-completion loop under "the experiment-tree model".
- **It's a sleep-until-change signal, not the source of truth.** It reports only
  completions it saw *during that one call*; a run that finishes between calls is
  already terminal next time and won't be reported. On every return, re-read
  `orx runs <projectId>` and act on *all* newly-terminal runs — don't treat the
  printed line as the complete set, and don't replace `exp wait` with a tight
  `orx runs` poll either (use `exp wait` to sleep, `orx runs` to reconcile).
- Call `--project` **while runs are in flight** (right after launching). If every
  run is already terminal, there's nothing left to complete, so it returns
  immediately printing `drained: no runs in flight` (exit 0) — your loop's
  termination signal.
- `--interval` is seconds between polls (default `5`); `--timeout` gives up after
  N seconds (default `1800`) and exits **non-zero** so callers can branch on it.
  For long training runs, raise `--timeout` (or treat a timeout exit as "nothing
  changed yet, loop again") so a wait that simply outlasts the interval isn't
  mistaken for an error.
- Progress lines go to **stderr**; the final completion line(s) go to **stdout**,
  each as `<runId> <prev> -> <status>` (or `<runId> <status> (new)`), or the
  single line `drained: no runs in flight` when nothing was in flight.

## Experiment description / notes — `orx exp desc`

Each experiment node carries a free-form **description** (markdown) — the same
field set by `create-experiment --description`. Use it for notes: observations,
hypotheses, or a running summary. It is a whole-document field: writing
overwrites whatever was there.

```sh
orx exp desc <expId>                          # print the description to stdout (empty → hint on stderr)
orx exp desc <expId> --set "tried lr=3e-4, diverged at step 4k"   # overwrite with a short note
cat notes.md | orx exp desc <expId> --stdin   # overwrite from stdin (long markdown)
```

- **Read** prints the text to **stdout** (pipe/redirect-friendly); when empty, a
  hint is printed to **stderr** and stdout stays empty.
- **Write** with exactly one of `--set` (inline) or `--stdin` (whole of stdin).
  Passing both is an error. Writing **replaces** the entire description — to
  append, read first, edit, and write back.
- `<expId>` comes from `orx experiments <projectId>` (the experiment id, not a run
  or project id).

## Reading committed code — `orx tree` / `orx cat` / `orx search`

These read an experiment's **committed branch** directly (via Forgejo) and need
**no open dev session** — use them to inspect a node's code without provisioning
compute. (They are distinct from the `orx ls`/`grep`/`read` dev-session verbs
below, which read the *live working tree* of an open dev node.)

```sh
orx tree <expId>                 # list every committed file
orx tree <expId> src             # list files under a path
orx cat  <expId> src/train.py    # print a committed file to stdout
orx search <expId> "batch_size"  # case-insensitive substring grep over the branch
```

- All read against the latest commit on the experiment's branch.
- `orx cat` writes content to **stdout** (pipe/redirect-friendly).

## Editing a node's files — `orx dev` sessions

To **change** files in an experiment, open a short-lived **dev session**: a CPU
node with the branch checked out. You edit the *live working tree* (no commits per
edit), then `dev close` makes **one commit** and tears the node down.

This is how you **realize a child's hypothesis**: after `create-experiment
--parent`, open a dev session on the child and make the specific code/config edits
its description calls for — then close and run. Edit only the files that idea
touches, and **don't touch the run command** (it's inherited; see "the
experiment-tree model" above). Edit children, never the baseline.

```sh
orx dev open <expId>                         # provisions a node (~30s), checks out the branch
orx ls   <expId> src                         # explore the working tree
orx read <expId> src/train.py
orx str-replace <expId> src/train.py "lr=1e-3" "lr=3e-4"
cat new_config.yaml | orx write <expId> config.yaml   # write content from stdin
orx grep <expId> "batch_size"
orx rm   <expId> stale_file.py
orx dev status <expId>                        # state + uncommitted changes
orx dev close <expId> -m "tune lr"            # ONE commit + push, then teardown
#   orx dev close <expId> --discard           # tear down without committing
```

Rules and notes:
- **Always `orx dev open` first.** The edit verbs (`read`/`write`/`str-replace`/`ls`/`grep`/`rm`)
  fail with a clear error if no dev node is open. (To read code *without* a dev
  node, use `orx tree`/`cat`/`search` above.)
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

## Run artifacts & code diffs — `orx artifacts` / `orx artifact` / `orx diff`

Beyond the terminal log, a run uploads **text artifacts** (eval outputs, reports,
generated files) and carries a **code diff** vs. its parent branch.

```sh
orx artifacts <runId>               # discover what a run uploaded (KEY + SIZE table)
orx artifact <runId> <key>          # read one artifact (tail by default)
orx artifact <runId> <key> --head --bytes 200000   # from the start, raise the cap
orx diff <runId>                    # unified diff of what this run's commit changed
```

- Start with `orx artifacts` to list keys, then `orx artifact <runId> <key>` to
  read one. Reading an artifact also **caches it for `orx query`** so you can grep
  artifact text via SQL.
- `orx artifact` content / `orx diff` diff go to **stdout**; byte-range and
  truncation metadata go to **stderr**.
- `orx diff` prints nothing (with a note on stderr) when the run's commit matches
  its parent branch.

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
  command reports that and exits non-zero.

## Literature search & paper content — `orx lit` / `orx paper`

These tap **alphaXiv's public corpus** (2.5M+ arXiv papers: CS, math, physics,
stats, q-bio/fin, EE — not PubMed/biomed). They need **no `orx login`** and hit
alphaXiv hosts, not the OpenResearch API. Use them to ground research in real
literature: find related work, pull a paper's structured report, and only drop to
its full text when you need a specific equation/table/section.

```sh
orx lit "speculative decoding for LLMs"            # ranked hits (id, title, date, votes, abstract)
orx lit "rotary position embeddings" --limit 10    # widen the result set (default 5)
orx lit "kv cache compression" --json              # raw JSON for programmatic use
orx paper 2401.12345                               # machine-readable report (the default)
orx paper https://arxiv.org/abs/2401.12345         # any arXiv/alphaXiv URL works too
orx paper 2401.12345v2 --full                      # full extracted text (fallback)
```

- **`orx lit`** prints, per hit: `<paperId>  <title>`, then `<date> · <votes> votes`,
  then a truncated abstract. The **`paperId`** is what you feed to `orx paper`.
  Results are relevance-ranked, capped at `--limit` (default 5). `--json` emits the
  raw hit objects (incl. matched `snippets`) for piping.
- **`orx paper <id>`** writes the report markdown to **stdout** (pipe/redirect-friendly).
  The id can be a bare id (`2401.12345`), a versioned id (`2401.12345v2`), or an
  arXiv / alphaXiv URL — the CLI normalizes it.
- **Report first, full text only when needed.** The default report is a compact
  (~10 KB) structured analysis and is enough for most questions. Reach for `--full`
  only when the report is missing a specific detail — it returns the entire paper.
- **404s are normal answers, not errors of yours.** A paper whose report hasn't been
  generated yet exits non-zero with a hint to try `--full`; one with no extracted
  text yet points you at the arXiv PDF. Try `--full`, then the PDF, before giving up.
- Override hosts with `ALPHAXIV_API_URL` (search) and `ALPHAXIV_WEB_URL` (paper docs)
  if you ever need to point elsewhere.

**Grounding a research loop in literature.** Before forming hypotheses for a project
(step 2 of the auto-research loop), search the literature for prior art on the knob
you're about to vary, pull the most relevant report, and let it inform the change you
write into a child's description:

```sh
orx lit "learning rate warmup schedules transformers" --limit 5
orx paper <bestPaperId>          # read its report; cite the idea in the child's --description
```

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

Orienting in a project (read-only discovery):

```sh
orx projects                     # find the project id
orx experiments <projectId>      # see the tree, pick an experiment id
orx skill project-query          # learn the evidence schema
orx query <projectId> "select title from experiments limit 10"
orx runs <projectId>             # find a run id
orx logs <runId>                 # read its output
```

To actually **drive** a project toward a goal — branch children off the baseline,
edit each child's code in a dev session, and keep the GPU budget saturated — follow
the auto-research loop in "the experiment-tree model" above.
