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
the standard way to read, diff, and edit it** (see "Reading & editing a node's
code"). Use `orx` when you need to inspect or drive project state from a shell
instead of the web UI.

## Cardinal rules — read before doing anything else

These four govern everything below. Breaking any one silently invalidates your
results — they are not style preferences. The detailed "experiment-tree model"
section expands on the why; these are the non-negotiables.

1. **Never edit the baseline (the root).** The root is the control every variant
   is measured against. To try an idea, **branch a child** and edit the child.
   Editing the root moves the goalposts and destroys every comparison.
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
   the failure mode. See "Shape the tree" below.

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

## Commands

### Auth
| Command | What it does |
|---|---|
| `orx login [--api-url <url>]` | Open a browser, do loopback OAuth, store a token. |
| `orx logout` | Remove the stored token. |

### Discover (project- and experiment-scoped)
| Command | What it does |
|---|---|
| `orx projects [--all]` | List your projects (id + name + GitHub `owner/repo`), grouped by org. `--all` includes archived. **Project ids and the repo to clone come from here.** |
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
| `orx chart wandb <projectId> --metric "<key>" --run <runId>[:label] ...` | Render a W&B metric across runs to a PNG line chart. See below. |
| `orx query <projectId> "<sql>"` | Run **one read-only DuckDB SQL statement** against the project's evidence schema. See below. |

### Create and run experiments (write)
| Command | What it does |
|---|---|
| `orx create-project <orgId> --name "<n>" [--repo <owner/repo>]` | Create a project: bound to a GitHub repo, or on a fresh blank repo when `--repo` is omitted. See below. |
| `orx create-experiment <projectId> --title "<t>" [...]` | Add an experiment node; prints its git branch. See below. |
| `orx compute [--gpu <id>] [--count <n>]` | List the GPU compute catalog (price-sorted). See below. |
| `orx exp status/cmd/run/cancel/wait <expId>` | Inspect, run, cancel, and wait on a single experiment node. `status` prints the node's branch, its parent's branch, the latest run's full commit SHA, and a ready-to-paste local `git diff` recipe. See below. |
| `orx exp desc <expId> [--set "<text>" \| --stdin]` | Read or overwrite the experiment's description (free-form notes). See below. |

To **read or edit** a node's code — including diffing what a run changed — use
plain git in the cache-dir clone; there is no `orx` code command. See "Reading &
editing a node's code" below.

### Literature & papers — alphaXiv (no login required)
| Command | What it does |
|---|---|
| `orx lit "<query>" [--limit <n>] [--json]` | Full-text search alphaXiv's paper corpus; returns ranked hits (id, title, date, votes, abstract). See below. |
| `orx paper <id\|url>` | Fetch a paper's **machine-readable report** (structured LLM-oriented analysis). Prints the paper's linked **GitHub repo** at the top when one exists. See below. |
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
**child** branched off a parent, inheriting its code and its run command. The two
rules this depends on — **never edit the baseline** and **the run command + env is
a fixed contract** — are the cardinal rules at the top of this skill; everything
below assumes them.

### Shape the tree — stacked bushes, not a flat fan or a noodle

The single most common way to drive a project badly is to get the **shape** wrong.
There are two opposite failures, and the right shape sits between them:

```
FLAT FAN (wrong)            NOODLE (wrong)            STACKED BUSHES (right)
root                        root                      root
├ a ├ b ├ c ... ├ n         └ a                       └ lr-head        ┐ round 1:
                              └ b                        ├ lr 2e-5     │ a small fan of
                                └ c                      └ lr 3e-5     ┘ co-equal options
                                  └ d ...                   └ winner ── arch-head   ┐ round 2
                                                               ├ arch-A             │ descends onto
                                                               └ arch-B             ┘ round 1's winner
```

- **Flat fan** (your whole sweep hanging off the root): every result is measured
  against the *start*, so wins never accumulate and the tree never makes progress.
- **Noodle** (a long single-child chain): depth manufactured for its own sake —
  each step doesn't actually build on the one above it.
- **Stacked bushes** (correct): a *small fan within a round* (the options of one
  decision), then **descend onto that round's winner** for the next round.

**The one rule that produces this shape.** Before you make X a child of Y, name
what Y established that X builds on:

- **You can name it** ("Y is the LR winner; X keeps that LR and changes the
  architecture") → real depth. X is a **child** of Y. Descend.
- **You can't — X and Y are co-equal options you're trying at the same time**
  (lr 2e-5 vs lr 3e-5) → they don't build on each other. They're **siblings** in
  the same bush. Fan, don't chain.

So: **width = the open options of one decision** (fan freely — a 3-way LR sweep
*should* be three siblings under a common head); **depth = decisions already
resolved, stacked** (one level down per winner kept). A new *round* never hangs off
the root — it hangs off the previous round's winner. That keeps the tree moving
**downward** as research progresses, without stringing unrelated nodes into a line.

Re-read `orx experiments` each round and check the shape: a wide row of direct
children off the root with no grandchildren means you're fanning when you should be
descending; a long depth-N chain with no branching means you're chaining co-equal
variants that should have been siblings.

### The auto-research loop

To drive a project toward a goal (e.g. "best convergence for d=8") under a fixed
GPU budget, this is the intended flow — do **not** edit the baseline or rewrite the
run command:

1. **Read the baseline's code.** Clone the project's repo into the cache dir and
   read it with your normal tools (see "Reading & editing a node's code" for the path).
   See its run command with `orx exp cmd <baseId>` and find where the knobs live
   (config files, hyperparameters, model defs).
2. **Form one round's worth of hypotheses** — the co-equal options of a *single*
   decision (which LR? which schedule? which init?), each a concrete change you can
   make and measure against the others in this round. Don't mix decisions from
   different rounds into one batch — that's what produces the flat fan.
3. **Create the round as a bush, and pick its parent deliberately.** All of this
   round's options are **siblings under one parent** — the title is the idea, the
   description is the concrete change you'll make on that node's branch. The parent is:
   - the **baseline**, only for the very first round (nothing has been won yet); or
   - the **previous round's confirmed winner**, for every round after — so this
     round's changes build *on top of* the last gain instead of resetting to the
     start. This is what walks the tree downward (see "Shape the tree" above).

   ```sh
   # Round 1 — one decision (the LR), its options fanned off the baseline:
   orx create-experiment <projectId> --parent <baseId> --title "LR 2e-5" \
     --description "Set the LR in config.yaml to 2e-5; change nothing else."
   orx create-experiment <projectId> --parent <baseId> --title "LR 3e-5" \
     --description "Set the LR in config.yaml to 3e-5; change nothing else."

   # Round 2 — LR 3e-5 won → the next decision (architecture) descends onto it:
   orx create-experiment <projectId> --parent <lr3e5WinnerId> --title "Wider MLP" \
     --description "On top of the LR-3e-5 winner, widen the MLP hidden dim 1024→2048 in model.py."
   ```
   The child inherits its parent's run command automatically — you don't set it,
   and you never give siblings different commands or env vars (cardinal rule 2).
4. **Implement each child's change on its git branch** — `orx create-experiment`
   prints the child's branch (`orx/<slug>`); sync the project's clone (in the
   openresearch cache dir — see "Reading & editing a node's code"), check the branch out,
   edit only the files that idea touches, commit, and push. **Leave the run
   command alone:**
   ```sh
   DIR=~/.cache/openresearch/repos/<owner>/<repo>   # owner/repo from `orx projects`
   [ -d "$DIR" ] || git clone https://github.com/<owner>/<repo> "$DIR"
   git -C "$DIR" fetch origin && git -C "$DIR" checkout -B orx/<child-slug> origin/orx/<child-slug>
   #   …edit config.yaml under "$DIR": schedule: constant → cosine …
   git -C "$DIR" commit -am "cosine LR + warmup" && git -C "$DIR" push
   ```
5. **Launch up to your GPU budget** — one run per ready child, in parallel:
   ```sh
   orx exp run <childId> --gpu H100_SXM --count 1
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
   `orx chart wandb …`, `orx query …`. To see exactly what a finished node
   changed, use the local git diff recipe `orx exp status <expId>` prints (see
   "Code diffs — local git"). Don't infer from status alone. Each
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
When you stop, consider writing up the tree as a local markdown report —
fetch `orx skill report` for the folder layout and section structure.

## `orx create-project` — start a new project

Creates a project in an organization (org ids come from `orx projects`). Every
project is backed by exactly one git repo; `--repo` picks where that repo comes
from:

```sh
# From an existing repo — yours (bound directly) or any readable repo
# (copied into a fresh repo the platform can write to):
orx create-project <orgId> --name "NanoGPT speedrun" --repo karpathy/nanoGPT

# From scratch — a fresh blank repo (just a stub root commit on main):
orx create-project <orgId> --name "My new idea"
```

- `--repo` takes `owner/repo` or a github.com URL. `--branch` (only with
  `--repo`) imports from a non-default branch. `--description` is optional.
- The new project has **no experiments yet** — it is an empty tree. Create the
  baseline (root node) next:
  ```sh
  orx create-experiment <projectId> --title "Baseline"
  ```
  For a blank project, check out the baseline's branch and push your starting
  code to it (see "Reading & editing a node's code" below) before launching runs.

## `orx create-experiment` — add a node to the tree

Adds a node to the experiment tree. `--title` is always required. The node shape
is chosen by flags:

```sh
# Child node, branched off an existing experiment:
orx create-experiment <projectId> --title "Larger batch size" --parent <experimentId>

# Baseline (root) node on the project's bound repo:
orx create-experiment <projectId> --title "Baseline"
```

- `--parent` selects the shape: with `--parent` ⇒ child; without it ⇒ baseline
  (root) on the repo the project is already bound to. The repo a project works on
  is chosen when the **project** is created (`orx create-project` or the web), so
  there is no `--repo` flag here.
- **A `--parent` child inherits the parent's run command** (and branches off its
  code). You do **not** set a run command on the child — keep it and vary the code
  on the child's git branch (see "the experiment-tree model" above).
- **Choose the parent to keep the tree descending, not the root.** Before you pass
  `--parent`, name what that parent established that this node builds on. The root
  is the right parent only for the *first* round; every later round's siblings hang
  off the **previous round's winner** (`orx experiments` shows the current shape).
  Piling round after round of children onto the root is the flat-fan failure (see
  "Shape the tree"). Co-equal options of the same decision are siblings under one
  parent — don't chain them into a line either.
- `--description` is optional but **strongly recommended for children**: use it to
  record the hypothesis / the concrete change this node makes. It's the node's
  free-form notes field (the same one `orx exp desc` reads/writes), and it's how
  you and the analysis tools tell sibling variants apart.

## Running an experiment — `orx exp` + `orx compute`

Each experiment node has a **run command** (the shell command that trains/evaluates
it) and is launched on **compute** you choose at run time. Compute is *not* stored
on the node — you pick a GPU (or an existing sandbox) each time you launch.

```sh
orx exp status <expId>                 # status, branch, parent, run command, latest run + commit, local diff recipe
orx exp cmd <expId>                    # print the current run command
orx exp cmd <baseId> --set "bash run.sh"   # set it ONCE on the baseline; children inherit it
orx compute                            # browse GPU offers (price-sorted)
orx compute --gpu H100_SXM --count 1   # filter the catalog
orx exp run <expId> --gpu H100_SXM --count 1 [--disk 100]     # launch on a NEW instance
orx exp run <expId> --sandbox <sandboxId>                 # launch on an EXISTING node
orx exp cancel <expId>                 # cancel the in-flight run
```

Rules and notes:
- **The run command is a fixed contract — set it once on the baseline, then leave
  it alone.** Children inherit it (see "the experiment-tree model" above). Don't
  `--set` a different command per child, and don't bake swept hyperparameters into
  it — vary the **code/config** on a child's git branch instead, so every variant
  runs the same command and their `EVAL.md`s stay comparable. The normal reason to
  touch a command is the baseline having none yet.
- **Set a run command before launching.** `orx exp run` fails with a pointer to
  `orx exp cmd --set` if the node has none.
- **Push your edits before launching.** A run trains the branch's tip **as it is
  on GitHub** — so commit and push first (see "Reading & editing a node's code"). As a
  safety net, `orx exp run` refuses a child whose branch has **no changes over its
  parent** (the tell-tale of "queued before pushing") — push and retry, or pass
  `--force` to run the unchanged code deliberately.
- **Pick compute with exactly one of `--gpu` or `--sandbox`.** With `--gpu`,
  `--count` defaults to `1` and `--disk` to `100` (GB). New instances are
  **RunPod-only** — the server picks the cheapest matching RunPod offer for the
  chosen (gpu, count); browse valid gpu ids and prices with `orx compute`.
- **GPU ids are exact enum strings, not family names.** `--gpu H100` is invalid —
  the variant suffix is part of the id (`H100_SXM`, `H100_PCIE`, `A100_SXM_80GB`,
  `RTX_4090`, …). Use the exact `GPU` column value from `orx compute`; run it
  first if unsure.
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

## Reading & editing a node's code — plain git in the cache-dir clone

Every experiment node **is a git branch** (`orx/<slug>`) on the project's GitHub
repo — `orx create-experiment` prints it. There is no dev box and no `orx` code
command: the **local clone in the cache dir is the standard way to interface
with code** — reading a node's files, diffing what a run changed, and editing —
all with plain git and your own tools.

**Clone into the openresearch cache dir, not your cwd.** The canonical location,
keyed by repo so the same clone is reused across all of a project's experiments:

```
~/.cache/openresearch/repos/<owner>/<repo>
```

`<owner>/<repo>` comes from `orx projects`. **Never** clone into your current
directory or the user's project folders — clones accreting in `~/projects` is the
failure mode this avoids.

This is how you **realize a child's hypothesis**: after `create-experiment
--parent`, check out the child's branch and make the specific code/config edits
its description calls for — then commit, push, and run. Edit only the files that
idea touches, and **don't touch the run command** (it's inherited; see "the
experiment-tree model" above). Edit children, never the baseline.

The sync recipe is **idempotent** — run it verbatim whether or not the clone
already exists from a previous session. Always fetch + reset before editing, so a
reused clone is never stale (and the experiment's branch, created server-side, is
fetched even when it's brand-new and not in your local clone yet):

```sh
DIR=~/.cache/openresearch/repos/<owner>/<repo>

# Clone once (skips if it already exists), then ALWAYS sync before touching a branch:
[ -d "$DIR" ] || git clone https://github.com/<owner>/<repo> "$DIR"
git -C "$DIR" fetch origin
git -C "$DIR" checkout -B orx/<slug> origin/orx/<slug>   # create, or reset to origin if it exists

#   …edit files under "$DIR" with your normal tools…
git -C "$DIR" commit -am "tune lr"     # one or more commits — your call
git -C "$DIR" push                     # push so runs and the tree see the change
```

Rules and notes:
- **Always sync first.** `git -C "$DIR" fetch origin && git -C "$DIR" checkout -B
  orx/<slug> origin/orx/<slug>` is mandatory every time — `-B …origin` creates the
  branch or resets an existing local one to the GitHub tip, so a persistent clone
  never edits a stale base. It discards uncommitted/unpushed local work on that
  branch, which is exactly what you don't want to carry across sessions (the
  contract is commit + push before moving on).
- **Auth is your own git.** Clone/push use whatever GitHub credentials your `git`
  already has — the repo lives under your account or your org, so access is the
  same as any of your repos. If a clone or push fails on auth, authenticate git
  for github.com (e.g. `gh auth login` or an SSH key) and retry.
- **Push before you run.** `orx exp run` launches from the branch's pushed tip on
  GitHub — uncommitted or unpushed edits won't be in the run. Commit and push
  first.
- **Reading another node's code** without disturbing your checkout: that branch is
  already in the clone after a fetch — `git -C "$DIR" show origin/orx/<slug>:<path>`.

### Code diffs — local git

What did a run change vs. its parent experiment? `orx exp status <expId>` prints
the parent's branch, the latest run's full commit SHA, and this exact recipe —
compute the diff locally in the same clone:

```sh
DIR=~/.cache/openresearch/repos/<owner>/<repo>   # owner/repo from `orx projects`
git -C "$DIR" fetch origin                        # ALWAYS fetch first — the commit and parent tip live on GitHub
git -C "$DIR" diff origin/<parent-branch>...<full-commit-sha>
```

- The **three-dot** form diffs from the merge-base — what the run's branch
  changed, not what the parent gained since the fork. That's the cumulative
  "what this experiment did to the code" view.
- Fetch first is mandatory: the run's commit and the parent's tip exist on
  GitHub and may not be in your clone yet.
- Root experiments have no parent — there is no diff base, by definition.

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
- For the run's **code diff**, use local git — see "Code diffs — local git" above.

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
- **The paper's code: `GitHub: <url>` line.** When alphaXiv has a GitHub repo linked
  to the paper, `orx paper` prints it as the first line (with `--full` too). If the
  report leaves you with questions about *how* something was actually implemented —
  exact hyperparameters, training loop details, a trick the paper glosses over —
  clone the repo into a temp dir and read the code:

  ```sh
  dir=$(mktemp -d) && git clone --depth 1 <githubUrl> "$dir"
  ```

  Inspect it there (grep for the model/optimizer setup, read the configs), and rely
  on it as the ground truth for reproducing the paper. No line means no repo is
  linked. Note the linked repo is the most-starred one associated with the paper —
  occasionally a big framework rather than the paper's own code; sanity-check the
  repo name before leaning on it.
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
orx skill report             # write a local markdown research report (with charts)
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
edit each child's code on its git branch, and keep the GPU budget saturated — follow
the auto-research loop in "the experiment-tree model" above.
