---
name: orx-create
description: "Create a project (`orx create-project`), seed an empty baseline from existing code, and add experiment nodes (`orx create-experiment`). Use when starting any new project or experiment, when the tree is empty, or when unsure how to bind a repo or set the run command."
---

## `orx create-project` — start a new project

Creates a project in an organization (org ids are printed next to the org names
in `orx projects`), bound to a git repo. The project starts with an **empty
experiment tree** — no baseline yet. Every project is backed by exactly one git
repo; `--repo` picks where that repo comes from:

```sh
# From an existing repo — yours (bound directly) or any readable repo
# (copied into a fresh repo the platform can write to):
orx create-project <orgId> --name "NanoGPT speedrun" --repo karpathy/nanoGPT

# From scratch — a fresh blank repo (just a stub root commit on main):
orx create-project <orgId> --name "My new idea"
```

- `--repo` takes `owner/repo` or a github.com URL. `--branch` (only with
  `--repo`) binds a non-default branch — the baseline will branch off it.
  `--description` is optional.
- The command prints the project id + repo. **Next step: create the baseline**
  (the root node, the control every variant is measured against):
  `orx create-experiment <projectId> --title "Baseline"` (no `--parent`).
  Run it once for reference numbers, then hang children off it with
  `orx create-experiment <projectId> --title "<t>" --parent <baselineId>`.
- For a **blank** project the baseline you create starts empty (a stub README):
  seed it from existing code before launching runs — for a paper or a known idea
  there is almost always a repo that implements it, and starting from that is
  faster and a far better control than code written from scratch. See "Seeding
  an empty baseline" below.

## Seeding an empty baseline — start from existing code, not from scratch

On a **blank** project, the baseline you create (`orx create-experiment`, no
`--parent`) starts as an empty stub (just a `README.md`) — there's no code to
run yet. The right move is almost never to **write the implementation by
hand.** For nearly any paper or idea there is already a repo that implements it,
and seeding the baseline from that repo is faster, more faithful, and a far
better control than something typed from memory. Reproductions should start from
the authors' (or a strong community) implementation, not a blank file.

This is the one legitimate time you put code *on the baseline itself* (cardinal
rule 1's only exception): it applies **only while the baseline is still the empty
stub.** Once it holds real code, the baseline is frozen — vary code on children
from then on.

**Find the code to seed from, in priority order:**

1. **The paper's own repo.** If the project has a paper (`orx project view` shows
   it, or you were given an arXiv id), run `orx paper <id>` — when alphaXiv has a
   repo linked it prints `GitHub: <url>` as the first line. That repo is the
   default seed. (Sanity-check the name: the linked repo is the most-starred one
   and is occasionally a big framework rather than the paper's own code.)
2. **No repo line, or the wrong repo? Search for one** — a missing `GitHub:` line
   means alphaXiv didn't have one indexed, *not* that none exists. Before falling
   back to scratch:
   - skim the paper's full text for a code/project URL: `orx paper <id> --full`
     (authors often link a repo or project page in the body or a footnote);
   - search the literature for the method and check related papers' repos:
     `orx lit "<the method>"` → `orx paper <hitId>` on the strongest hits and read
     their `GitHub:` lines.
3. **No paper at all (a free-form idea project)?** Treat the idea as the query:
   `orx lit "<the idea>"`, read the most relevant report with `orx paper`, and
   seed from the best implementation it points to. Only if a genuine search turns
   up nothing usable do you start from a minimal scaffold of your own — and say so
   in the baseline's description.

**Seed the baseline branch from the chosen repo.** Work in the cache-dir clone
(see the `orx-git` skill); replace the stub with the source's code
as one squashed commit, so the baseline keeps clean provenance and stays rooted
on the project repo:

```sh
DIR=~/.cache/openresearch/repos/<owner>/<repo>          # the PROJECT's repo, from `orx projects`
[ -d "$DIR" ] || git clone https://github.com/<owner>/<repo> "$DIR"
git -C "$DIR" fetch origin
git -C "$DIR" checkout -B orx/<baseline-slug> origin/orx/<baseline-slug>   # the baseline's branch

src=$(mktemp -d) && git clone --depth 1 https://github.com/<srcOwner>/<srcRepo> "$src"
SHA=$(git -C "$src" rev-parse --short HEAD) && rm -rf "$src/.git"
find "$DIR" -mindepth 1 -maxdepth 1 ! -name .git -exec rm -rf {} +   # drop the stub
cp -a "$src/." "$DIR/"                                               # lay down the source tree
git -C "$DIR" add -A
git -C "$DIR" commit -m "Seed baseline from <srcOwner>/<srcRepo>@$SHA"
git -C "$DIR" push
```

Then make the baseline runnable and proceed normally:

- read the seeded code, find its entry point, and set the run command **once**:
  `orx exp cmd <baselineId> --set "bash run.sh"` (rule 2's one legitimate `--set`);
- run the baseline first for a control `EVAL.md`, then branch children and vary
  code per the auto-research loop. The baseline is **frozen** the moment it holds
  real code — shrink to the smallest config that still shows the paper's claim by
  editing a **child**, never by trimming the root.

## `orx create-experiment` — add a node to the tree

Adds a node to the experiment tree. `--title` is always required. The node shape
is chosen by flags:

```sh
# Child node, branched off an existing experiment:
orx create-experiment <projectId> --title "Larger batch size" --parent <experimentId>

# Baseline (root) node on the project's bound repo:
orx create-experiment <projectId> --title "Baseline"

# Additional baseline (another root) when the project already has one:
orx create-experiment <projectId> --title "Baseline v2" --baseline
```

- `--parent` selects the shape: with `--parent` ⇒ child; without it, on an
  **empty project**, ⇒ the baseline (root) on the repo the project is already
  bound to. Once a root exists, a parentless create attaches under the oldest
  root on local projects (server projects create another baseline); pass
  `--baseline` to explicitly add another root — projects may hold multiple
  baselines, each the control for its own subtree. The repo a project works
  on is chosen when the **project** is created (`orx create-project` or the
  web), so there is no `--repo` flag here.
- **A `--parent` child inherits the parent's run command** (and branches off its
  code). You do **not** set a run command on the child — keep it and vary the code
  on the child's git branch (see the `orx-experiment-tree` skill).
- **Choose the parent to keep the tree descending, not the root.** Before you pass
  `--parent`, name what that parent established that this node builds on. The root
  is the right parent only for the *first* round; every later round's siblings hang
  off the **previous round's winner** (`orx experiments` shows the current shape).
  Piling round after round of children onto the root is the flat-fan failure (see
  "Shape the tree" in the `orx-experiment-tree` skill). Co-equal options of the
  same decision are siblings under one parent — don't chain them into a line either.
- `--description` is optional but **strongly recommended for children**: use it to
  record the hypothesis / the concrete change this node makes. It's the node's
  free-form notes field (the same one `orx exp desc` reads/writes), and it's how
  you and the analysis tools tell sibling variants apart.
