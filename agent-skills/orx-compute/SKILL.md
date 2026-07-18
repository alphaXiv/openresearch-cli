---
name: orx-compute
description: "Launch experiment runs with `orx exp run`: backends (hf, modal, k8s, ssh, slurm, openresearch, local), flavors, timeouts, images, sizing, and `orx exp wait`. Use before launching or re-launching any run, when choosing or switching a backend or GPU flavor, when a job OOMs, stalls, or times out, or when deciding GPU vs CPU."
---

Each experiment node has a **run command** (the shell command that trains/evaluates
it) and is launched on **compute** you choose at run time. Compute is *not* stored
on the node — you pick a GPU, a CPU-only instance, or an existing sandbox each time
you launch (local projects may instead carry a default target — see "The default
compute target" below).

```sh
orx exp status <expId>                 # status, branch, parent, run command, latest run + commit, local diff recipe
orx exp cmd <expId>                    # print the current run command
orx exp cmd <baseId> --set "bash run.sh"   # set it ONCE on the baseline; children inherit it
orx compute                            # browse GPU offers across all providers (price-sorted)
orx compute --gpu H100_SXM --count 1   # filter by gpu / count
orx compute --provider vast            # filter by provider
orx compute --cpu                      # browse CPU-only offers (price-sorted)
orx exp run <expId> --gpu H100_SXM --count 1 [--disk 100]     # launch on a NEW GPU instance (RunPod)
orx exp run <expId> --gpu H100_SXM --provider vast       # launch on another provider's GPU
orx exp run <expId> --cpu cpu5c --vcpus 8                 # launch on a NEW CPU-only instance
orx exp run <expId> --sandbox <sandboxId>                 # launch on an EXISTING node
orx exp cancel <expId>                 # cancel the in-flight run
```

Rules and notes:
- **The run command is a fixed contract — set it once on the baseline, then leave
  it alone.** Children inherit it (see the `orx-experiment-tree` skill). Don't
  `--set` a different command per child, and don't bake swept hyperparameters into
  it — vary the **code/config** on a child's git branch instead, so every variant
  runs the same command and their `EVAL.md`s stay comparable. The normal reason to
  touch a command is the baseline having none yet.
- **Set a run command before launching.** `orx exp run` fails with a pointer to
  `orx exp cmd --set` if the node has none.
- **Push your edits before launching.** A run trains the branch's tip **as it is
  on GitHub** — so commit and push first (see the `orx-git` skill). As a
  safety net, `orx exp run` refuses a child whose branch has **no changes over its
  parent** (the tell-tale of "queued before pushing") — push and retry, or pass
  `--force` to run the unchanged code deliberately.
- **Pick compute with exactly one of `--gpu`, `--cpu`, or `--sandbox`.** With
  `--gpu`, `--count` defaults to `1` and `--disk` to `100` (GB). A new GPU
  instance defaults to **RunPod** (the cheapest matching RunPod offer for the
  chosen gpu/count); pass `--provider <name>` to launch from another provider
  shown in `orx compute` (e.g. `vast`, `lambda`). New CPU instances are
  RunPod-only. Browse valid gpu ids, providers, and prices with `orx compute`.
- **GPU ids are exact enum strings, not family names.** `--gpu H100` is invalid —
  the variant suffix is part of the id (`H100_SXM`, `H100_PCIE`, `A100_SXM_80GB`,
  `RTX_4090`, …). Use the exact `GPU` column value from `orx compute`; run it
  first if unsure.
- **Use `--cpu` for GPU-less work** (data prep, eval harnesses, CPU-bound papers).
  The flavor is one of `cpu5c` (compute), `cpu5g` (general), or `cpu5m` (memory);
  `--vcpus` is one of `2`, `8`, `32` (default `8`). Browse offers with
  `orx compute --cpu`. CPU instances size their disk from the vCPU count, so there
  is no `--disk` knob.
- `orx exp run` **queues** the run and returns immediately — it does not wait.
  Follow progress with `orx runs <projectId>` and `orx logs <runId>`, or block
  with `orx exp wait` (below).

## The default compute target (local projects)

The user may set a **default compute target** in the `orx up` dashboard
(Settings → Compute → Make default); it is machine-wide and applies to every
local project. When one is set, `orx exp run <expId>` with no `--backend`
launches there, with the saved default flavor — omitting the flag is how you
use it (flavor-required backends still need `--flavor` if no default flavor
is saved). When none is set, local launches require an explicit `--backend`; the
compute choice is the user's, so ask. Server projects are unaffected: managed
compute (`--gpu`/`--cpu`/`--sandbox`) stays their default.

## Running on Hugging Face Jobs — `--backend hf`

**Managed compute (`--gpu`/`--cpu`/`--sandbox`) is the default. Use
`--backend hf` ONLY when the user explicitly asks for Hugging Face Jobs**
(e.g. "run this on HF", "use my huggingface account"), it is the configured
default target (orx up Settings → Compute), or the project context says to
prefer it. A connected HF token by itself is NOT a signal to switch — it just
means the option exists. When in doubt, launch on managed compute.

With `--backend hf`, the job runs on the user's own HF account (requires
`HF_TOKEN` in the environment — orgs that connect their HF account in compute
settings get it synced automatically) and is billed there per minute; no
OpenResearch balance is spent.

```sh
orx exp run <expId> --backend hf --flavor a10g-small              # one GPU job
orx exp run <expId> --backend hf --flavor a100-large --timeout 8h
orx exp run <expId> --backend hf --flavor cpu-upgrade --image python:3.12
```

Rules and notes:
- **`--flavor` is required** and replaces `--gpu`/`--cpu`/`--sandbox`. Common
  flavors: `t4-small`, `a10g-small`, `a10g-large`, `l4x1`, `l40sx1`,
  `a100-large`, `h200` (and `x2/x4/x8` multiples); CPU: `cpu-basic`,
  `cpu-upgrade`. Prefer the smallest flavor that fits — same sizing discipline
  as managed GPUs.
- **Set `--timeout` to cover the whole run** (default `4h`). HF kills the job
  at the timeout; a killed job reads as a failed run.
- The job clones the experiment branch's **GitHub tip** and runs the fixed run
  command, same contract as managed runs — commit and push first. Private
  repos work automatically: the platform mints a repo-scoped clone token from
  the project's connected GitHub app and passes it to the job as a secret.
  Never ask the user to provision a `GITHUB_TOKEN`; setting one (env or
  project env var) is only an override for repos outside the connected app.
- `--image` overrides the container (default: a CUDA pytorch image on GPU
  flavors, `python:3.12` on cpu flavors). Pick an image with your deps baked
  in when pip-install time dominates the run.
- Everything downstream is identical: the run appears in the tree, `orx exp
  wait` / `orx runs` / `orx logs` work unchanged, and cancel from the web or
  `orx exp cancel` reaches the job within a few seconds. A detached
  `orx supervise` process mirrors status and logs; don't kill it.

## Running on Modal — `--backend modal`

**Same rule as HF: managed compute is the default. Use `--backend modal` ONLY
when the user explicitly asks for Modal** ("run this on Modal", "use my Modal
account") or it is the configured default target. Modal runs on the user's
own Modal account, billed there per second; no OpenResearch balance is spent.
It runs the job in a Modal **Sandbox** (an ephemeral container that scales to
zero when the run ends).

orx auto-provisions a managed `modal` environment on the first Modal launch (no
pip-install needed). You only need a Modal token — `MODAL_TOKEN_ID` +
`MODAL_TOKEN_SECRET` in the environment (set them as org or project env vars and
they sync to the box automatically), or `modal token new`.

```sh
orx exp run <expId> --backend modal --flavor a10g               # one GPU sandbox
orx exp run <expId> --backend modal --flavor a100-80gb --timeout 8h
orx exp run <expId> --backend modal --flavor h100:2             # 2× H100
orx exp run <expId> --backend modal --flavor cpu --image python:3.12
```

Rules and notes:
- **`--flavor` is required** and replaces `--gpu`/`--cpu`/`--sandbox`. It's a
  Modal GPU: `t4`, `l4`, `a10g`, `a100`, `a100-80gb`, `l40s`, `h100`, `h200`
  (append `:N` for a count, e.g. `h100:2`); or `cpu` / `cpu-large` for CPU-only.
  Prefer the smallest flavor that fits.
- **Set `--timeout` to cover the whole run** (default `4h`). Modal kills the
  sandbox at the timeout; a killed sandbox reads as a failed run.
- Same clone contract as HF/managed: the sandbox clones the experiment branch's
  **GitHub tip** and runs the fixed command — commit and push first. Private
  repos work automatically via the platform's repo-scoped clone token.
- `--image` overrides the container (default: a CUDA pytorch image on GPU
  flavors, `python:3.12` on cpu). Pick one with your deps baked in when
  pip-install time dominates.
- Everything downstream is identical (`orx exp wait` / `orx runs` / `orx logs`,
  cancel from web or `orx exp cancel`). A detached `orx supervise` mirrors
  status and logs; don't kill it.

## Running on your Kubernetes cluster — `--backend k8s`

Runs the experiment on your own Kubernetes cluster from a manifest committed on
the experiment branch. The full manifest contract lives in the `orx-compute-k8s`
skill — fetch it (`orx skill compute-k8s`) before your first k8s launch.

## Running on your own box — `--backend ssh`

**Same rule: use `--backend ssh` ONLY when the user explicitly asks to run on
their own machine/server** ("run this on my box", "use my GPU server") or it
is the configured default target (`--host <alias>` is still required per
launch). Local projects (`orx up`) only for now. It runs the experiment as a
detached background process on a host from your `~/.ssh/config`, over `ssh` —
no scheduler, no container, the host's own environment.

```sh
orx exp run <expId> --backend ssh --host my-gpu-box     # ~/.ssh/config alias
```

Rules and notes:
- **`--host` is the ssh host alias** (from `~/.ssh/config`) — a machine, not a
  hardware shape, so there is no `--flavor` here. See `orx up` Settings →
  Compute → SSH (each host has a "Test" button that checks reachability + git).
- Auth is your ssh keys/agent — orx never reads a key, it just shells out to
  `ssh <alias>`. The host needs `git` and `bash`; it clones the experiment
  branch's GitHub tip (private repos via the `GITHUB_TOKEN` passed in the run's
  env) and runs the fixed command. Commit and push first, same as the others.
- No `--image` (the host's environment is used as-is) and no `--timeout` (the
  process runs until it exits or you cancel).
- The run lives under `~/.orx/runs/<runId>/` on the host (`run.sh`, `log`,
  `pid`, `exit_code`). Cancel from the web or `orx exp cancel` kills the remote
  process group. Everything downstream (`orx exp wait` / `runs` / `logs`) is
  identical; a detached `orx supervise` polls it over ssh — don't kill it.

## Running on your Slurm cluster — `--backend slurm`

**Same rule: use `--backend slurm` ONLY when the user explicitly asks for their
Slurm cluster** ("submit it to the cluster", "run it via sbatch") or it is the
configured default target. Local projects (`orx up`) only. It submits the
experiment as a batch job via `sbatch` on the login node, reached over ssh — the
host's environment as-is, no container.

```sh
orx exp run <expId> --backend slurm --host login-node --flavor h100:2 --timeout 4h
orx exp run <expId> --backend slurm                    # CPU-only, settings default host
```

Rules and notes:
- **`--host` is the login node's `~/.ssh/config` alias**; omit it to use the
  default from the slurm settings (`orx up` Settings → Compute → Slurm).
- **`--flavor` is a GRES GPU request** (`h100:2` = two H100s) — omit it for a
  CPU-only job. There is no `--image`; the job runs in the cluster's own
  environment (modules, conda, whatever the login profile provides).
- `--timeout` (default `4h`) applies — size it to cover the whole run; a job
  killed at the timeout reads as a failed run.
- Same clone contract as every backend: the job clones the experiment branch's
  GitHub tip and runs the fixed command — commit and push first. Everything
  downstream (`orx exp wait` / `orx runs` / `orx logs` / `orx exp cancel`) is
  identical; a detached `orx supervise` mirrors status and logs — don't kill it.

## Running on an OpenResearch box — `--backend openresearch`

**Same rule: use `--backend openresearch` ONLY when the user explicitly asks
for it** ("use an openresearch box", "bill it to the org") or it is the
configured default target. It provisions an **ephemeral OpenResearch machine
billed to the user's org** — created for this run and deleted when it ends —
with a fixed CUDA + PyTorch + uv image. Needs `orx login` and a registered SSH
key.

```sh
orx exp run <expId> --backend openresearch --flavor h100_sxm:2 --timeout 4h
orx exp run <expId> --backend openresearch --flavor cpu5c:32 --org <orgId>
```

Rules and notes:
- **`--flavor` is a GPU id from `orx compute`** (`h100_sxm`, `h100_sxm:2` for a
  count) **or a CPU flavor** (`cpu5c` / `cpu5g` / `cpu5m`, with `:vcpus` like
  `cpu5c:32`). Run `orx compute` to see what's available.
- Optional: `--org <id>` (when you belong to several), `--disk <GB>`, and
  `--provider <P>`. No `--image` — the platform's image is fixed.
- `--timeout` (default `4h`) applies — the box is deleted when the run ends
  either way, so nothing persists on it; everything you need must be in the log.
- Same clone contract and downstream commands as every backend; a detached
  `orx supervise` mirrors status and logs — don't kill it.

## Running on this machine — `--backend local`

**Same rule: use `--backend local` ONLY when the user explicitly asks to run
locally** ("run it on this machine", "just run it here") or it is the
configured default target. Local projects (`orx up`) only. It runs the
experiment as a detached background process on the machine running orx — no
scheduler, no container, this machine's own environment.

```sh
orx exp run <expId> --backend local
```

Rules and notes:
- **Nothing to pick** — no `--flavor`, `--host`, `--image`, or `--timeout`
  (the process runs until it exits or you cancel). The hardware is whatever
  this machine has; prefer it for small or CPU-scale runs and use a remote
  backend for anything heavy — it shares CPU/RAM/GPU with everything else on
  the machine.
- Same clone contract as every backend: the run clones the experiment
  branch's GitHub tip into its own run dir (never your checkout) and runs the
  fixed command — commit and push first. Never run training directly in your
  shell instead: that would be unsupervised and invisible to the dashboard.
- The run lives under `<orx data dir>/local-runs/<runId>/` (`run.sh`, `log`,
  `pid`, `exit_code`). Cancel from the web or `orx exp cancel` TERMs the
  process group. Everything downstream (`orx exp wait` / `orx runs` / `orx logs`) is
  identical; a detached `orx supervise` watches it — don't kill it.

## Spinning up a standalone instance — `orx instance create`

Provision a persistent instance in an **organization**, not tied to any
experiment — the CLI equivalent of the dashboard's org "Spin up" panel. Use this
for ad-hoc/manual compute (you SSH in yourself); experiment runs use `orx exp run`
instead.

```sh
orx instance create <orgId> --gpu H100_SXM --count 1 [--disk 100]   # GPU box (cheapest provider)
orx instance create <orgId> --gpu H100_SXM --provider runpod        # pin a provider
orx instance create <orgId> --cpu cpu5g --vcpus 8                    # CPU-only box
```

- `<orgId>` comes from `orx projects` (the `org:` line). The flags mirror
  `orx exp run`: exactly one of `--gpu` or `--cpu`; `--count`/`--disk` apply to
  `--gpu`, `--vcpus` to `--cpu`.
- Unlike `orx exp run`, omitting `--provider` picks the **cheapest** matching
  offer across all providers; pass `--provider <name>` to pin one.
- The box provisions asynchronously — the command prints its id and current
  status; its SSH host appears once it's online.

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
  each freed slot. See the per-completion loop in the `orx-experiment-tree` skill.
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
- **When a run is `failed`, a `reason:` line follows it.** Compute spin-up
  failures (no GPU capacity, provider quota/limit hit, transient provider error)
  carry the provider's own message here — the same text the website shows as a
  toast. These are usually **transient and retryable**: wait and re-launch the
  same run, or pick a different GPU/provider, rather than treating the experiment
  as a dead end. If the run instead failed *after* the box came up, the `reason:`
  line points at `orx logs <runId>`, where the traceback/OOM/setup error lives.
  The same `reason:` line appears under `orx exp status <expId>` and beneath the
  `orx runs <projectId>` table.

## Sizing compute

- **Decide GPU vs CPU first.** API-driven evals, data prep, and CPU-bound
  papers run fine (and far cheaper) on a CPU flavor.
- **Pick the smallest flavor that fits** the model and a minimal batch; don't
  reflexively grab the biggest.
- **Let a real failure escalate you.** OOM or hopelessly-slow → move up a
  tier. That's expected, not a mistake.
- Raise `--timeout` (`--timeout 1d`) only for genuinely long runs.
