A project is a **tree of experiment nodes**. The root (**baseline**) holds the
starting code and a **run command** — the single shell command that trains or
evaluates the node and writes an `EVAL.md` with its results. Every other node is a
**child** branched off a parent, inheriting its code and its run command. The two
rules this depends on — **never edit the baseline** and **the run command + env is
a fixed contract** — are the cardinal rules; everything below assumes them.

## Shape the tree — stacked bushes, not a flat fan or a noodle

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

## The auto-research loop

To drive a project toward a goal (e.g. "best convergence for d=8") under a fixed
GPU budget, this is the intended flow — do **not** edit the baseline or rewrite the
run command:

1. **Read the baseline's code.** Clone the project's repo into the cache dir and
   read it with your normal tools (see the `orx-git` skill for the path).
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
   openresearch cache dir — see the `orx-git` skill), check the branch out,
   edit only the files that idea touches, commit, and push. **Leave the run
   command alone:**
   ```sh
   DIR=~/.cache/openresearch/repos/<owner>/<repo>   # owner/repo from `orx projects`
   [ -d "$DIR" ] || git clone https://github.com/<owner>/<repo> "$DIR"
   git -C "$DIR" fetch origin && git -C "$DIR" checkout -B orx/<child-slug> origin/orx/<child-slug>
   #   …edit config.yaml under "$DIR": schedule: constant → cosine …
   git -C "$DIR" commit -am "cosine LR + warmup" && git -C "$DIR" push
   ```
   While you're in the code, **make the run emit the evidence you'll need to judge
   it.** Have it write rollout transcripts, per-sample eval breakdowns, generated
   text, or summary tables to `.openresearch/artifacts/` (as text — JSONL/CSV/md) —
   a directory at the **repo root**, where the run command's working dir points, so
   `.openresearch/artifacts/foo.jsonl` is a plain relative path (if your code `cd`s
   into a subdir or writes to `/tmp` first, resolve it from the repo root instead).
   Those files sync to storage and become readable in step 7 via `orx artifacts` /
   `orx artifact`. A run you can only read the tail-log of is far weaker evidence
   than one that dumped its rollouts. See the `orx-evidence` skill.
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
   the `orx-git` skill). Don't infer from status alone. Each
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
When you stop, consider writing up the tree as a local markdown report — see the
`orx-reports` skill for the folder layout and section structure.

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
