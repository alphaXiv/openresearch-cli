---
name: orx-compute
description: "Launch experiment runs with `orx exp run`: backends (hf, modal, k8s, ssh, slurm, ray, openresearch, local), flavors, timeouts, images, sizing, and `orx exp wait`. Use before launching or re-launching any run, when choosing or switching a backend or GPU flavor, when a job OOMs, stalls, or times out, or when deciding GPU vs CPU."
---

Commit the experiment before launching. `orx` archives that exact revision and
transfers the immutable snapshot directly to the selected backend. No push,
repository clone, GitHub token, or public repository is required.

In local mode (`orx up`) every run launches with `orx exp run <expId>` onto a
**backend**: `hf`, `modal`, `k8s`, `ssh`, `slurm`, `ray`, `openresearch`, or `local`.
There is no managed-SKU compute here (`--gpu`/`--cpu`/`--sandbox` are
server-project flags) — the backend comes from an explicit `--backend`, or from
the machine-wide default target configured by the user.

```sh
orx exp status <expId>                     # status, branch, run command, latest run
orx exp run <expId> --backend <b> [flags]  # launch (omit --backend when a default target is set)
orx exp cancel <expId>                     # cancel the in-flight run
```

## The contract every backend shares

- **The run command is a fixed contract — set it once, then leave it alone.**
  It lives on the project (`orx project edit <projectId> --run-command '<cmd>'`,
  or `--run-command` on the first `create-experiment`) and children inherit it.
  Don't vary it per child and don't bake swept hyperparameters into it — vary
  code/config on the child's branch instead (see the `orx-experiment-tree`
  skill). A node with no command refuses to launch and points at
  `orx project edit`.
- **Commit before launching.** Every backend runs the same content-addressed
  archive of the recorded local revision. Uncommitted edits are excluded; a
  push is unrelated to launching (see the `orx-git` skill).
- **`orx exp run` queues the run and returns immediately** — it does not wait.
  Follow progress with `orx runs <projectId>` and `orx logs <runId>`, or block
  with `orx exp wait` (below).
- **Everything downstream is identical on every backend**: `orx exp wait` /
  `orx runs` / `orx logs` / `orx exp cancel` work unchanged, and a detached
  `orx supervise` process mirrors status and logs — never kill it.

## Choosing a backend, and the default target

The user may configure a **default compute target**; it is machine-wide and
applies to every local project.
When one is set, `orx exp run <expId>` with no `--backend` launches there with
the saved default flavor — omitting the flag is how you use it (flavor-required
backends still need `--flavor` if no default flavor is saved; ssh always needs
`--host`). When none is set, the backend choice is the **user's**: if the task
doesn't name one and the user hasn't picked one in this conversation, ask
before launching. A connected token (HF, Modal, …) is NOT a signal to pick that
backend — it just means the option exists.

## Hugging Face Jobs — `--backend hf`

Runs on the user's own HF account (needs `HF_TOKEN` in the environment), billed
there per minute.

```sh
orx exp run <expId> --backend hf --flavor a10g-small
orx exp run <expId> --backend hf --flavor a100-large --timeout 8h
orx exp run <expId> --backend hf --flavor cpu-upgrade --image python:3.12
```

- **`--flavor` is required.** Common flavors: `t4-small`, `a10g-small`,
  `a10g-large`, `l4x1`, `l40sx1`, `a100-large`, `h100`, `h200` (and `x2/x4/x8`
  multiples); CPU: `cpu-basic`, `cpu-upgrade`. Prefer the smallest that fits.
- **Set `--timeout` to cover the whole run** (default `4h`) — HF kills the job
  at the timeout, and a killed job reads as a failed run.
- `--image` overrides the container (default: a CUDA pytorch image on GPU
  flavors, `python:3.12` on CPU). Pick an image with your deps baked in when
  pip-install time dominates the run.

## Modal — `--backend modal`

Runs in a Modal **Sandbox** (an ephemeral container that scales to zero when
the run ends) on the user's own Modal account, billed per second. Needs
`MODAL_TOKEN_ID` + `MODAL_TOKEN_SECRET` in the environment (or
`modal token new`); orx auto-provisions its managed `modal` environment on the
first launch.

```sh
orx exp run <expId> --backend modal --flavor a10g
orx exp run <expId> --backend modal --flavor h100:2 --timeout 8h
orx exp run <expId> --backend modal --flavor cpu --image python:3.12
```

- **`--flavor` is required**: `t4`, `l4`, `a10g`, `a100`, `a100-80gb`, `l40s`,
  `h100`, `h200` (append `:N` for a count, e.g. `h100:2`), or `cpu` /
  `cpu-large` for CPU-only.
- `--timeout` (default `4h`) and `--image` behave exactly as on hf.

## Your Kubernetes cluster — `--backend k8s`

The run's shape is a Kubernetes manifest committed on the experiment branch
(default `.orx/k8s.yaml`) — no flavors, no `--image`. The full manifest
contract lives in the **`orx-compute-k8s`** skill; load it before your first
k8s launch.

## Your own box — `--backend ssh`

A detached process on a host from your `~/.ssh/config` — no scheduler, no
container, the host's environment as-is.

```sh
orx exp run <expId> --backend ssh --host my-gpu-box
```

- **`--host <alias>` is required on every launch** — a machine, not a hardware
  shape, so there is no `--flavor`, `--image`, or `--timeout` (the process runs
  until it exits or is cancelled). Use one of the user's configured SSH host
  aliases; OpenResearch validates reachability before upload.
- Auth is your own ssh keys/agent — orx never reads a key, it just shells out
  to `ssh <alias>`. The host needs `tar` and `bash`; orx uploads the snapshot
  over SSH. The run lives under
  `~/.orx/runs/<runId>/` on the host; cancel kills the remote process group.

## Your Slurm cluster — `--backend slurm`

Submitted as a batch job via `sbatch` on the login node, reached over ssh — the
cluster's own environment (modules, conda, whatever the login profile
provides), no container.

```sh
orx exp run <expId> --backend slurm --host login-node --flavor h100:2 --timeout 4h
orx exp run <expId> --backend slurm                    # CPU-only, settings default host
```

- **`--host` is the login node's `~/.ssh/config` alias**; omit it to use the
  configured Slurm default. **`--flavor` is a GRES GPU request**
  (`h100:2` = two H100s); omit it for a CPU-only job. There is no `--image`;
  `--timeout` (default `4h`) applies — size it to cover the whole run.

## A Ray Jobs cluster — `--backend ray`

Submits via the Ray Jobs / Dashboard API. The snapshot is uploaded as Ray's
`working_dir` package. Needs a reachable Ray head (Dashboard, usually port
8265).

```sh
orx exp run <expId> --backend ray
orx exp run <expId> --backend ray --flavor gpu:1
orx exp run <expId> --backend ray --flavor cpu:2,mem:8GiB
```

- **Address** comes from the user's saved Ray configuration, else
  `ASTROAI_RAY_JOBS_ADDRESS` / `RAY_DASHBOARD_URL`, else
  `http://127.0.0.1:8265`.
- **`--flavor` is optional** resource hints for the job entrypoint:
  `cpu[:N]`, `gpu[:N]`, `mem:<size>` (comma-separated; `mem` is a scheduling
  reservation, not an enforced cap). Omit it to reserve nothing (avoids
  Pending on small heads). No `--image`, `--host`, or `--timeout` — the job
  runs in the cluster's runtime env until it finishes.

## An OpenResearch box — `--backend openresearch`

Provisions an **ephemeral OpenResearch machine billed to the user's org** —
created for this run and deleted when it ends — with a fixed CUDA + PyTorch +
uv image. Needs `orx login` and an SSH key registered from *this* computer —
without one the box comes online but refuses the connection.

`orx ssh-key list` marks which registered keys are usable here. If none are,
register this computer's: `orx ssh-key add <path to a .pub in ~/.ssh>` — pass
the actual file (there may be several, or none, in which case create one with
`ssh-keygen -t ed25519 -f ~/.ssh/id_ed25519` first). `orx exp run` refuses to
provision when nothing is registered and names the file to add.

```sh
orx exp run <expId> --backend openresearch --flavor h100_sxm:2 --timeout 4h
orx exp run <expId> --backend openresearch --flavor cpu5c:32 --org <orgId>
```

- **`--flavor` is a GPU id from `orx compute`** (`h100_sxm`, `h100_sxm:2` for a
  count) or a CPU flavor (`cpu5c` / `cpu5g` / `cpu5m`, with `:vcpus` like
  `cpu5c:32`). Optional: `--org <id>`, `--disk <GB>`, `--provider <P>`. No
  `--image` — the platform's image is fixed.
- The box is deleted when the run ends either way — nothing persists on it, so
  everything you need must be in the log. `--timeout` (default `4h`) applies.

## This machine — `--backend local`

A detached, supervised process on the machine running `orx up` — no scheduler,
no container, this machine's environment as-is.

```sh
orx exp run <expId> --backend local
```

- **No flags** — no `--flavor`, `--host`, `--image`, or `--timeout` (the
  process runs until it exits or is cancelled); the hardware is whatever this
  machine has. It shares CPU/RAM/GPU with OpenResearch and your editing tools:
  prefer it for small or CPU-scale runs and a remote backend for anything
  heavy.
- Still the full run contract: it extracts the recorded snapshot into its own run dir
  (never your checkout), supervised and tracked by OpenResearch — never run
  training directly in your shell instead. The run lives under
  `<orx data dir>/local-runs/<runId>/`; cancel TERMs the process group.

## Waiting on runs — `orx exp wait`

Block until a run changes state — useful when driving a research loop and you
want to act as soon as a run finishes. Two modes, picked by argument:

```sh
orx exp wait <expId>                    # level trigger: poll this experiment's latest run
                                        #   until it reaches a terminal state (done/failed/cancelled)
orx exp wait --project <projectId>      # edge trigger: return when the FIRST run in the
                                        #   project COMPLETES (transitions into done/failed/cancelled)
orx exp wait <expId> --interval 10 --timeout 3600   # tune polling
```

- Pass **exactly one** of `<expId>` or `--project` (not both, not neither).
- `--project` is the **budget-loop** primitive: it wakes only on a
  **completion** (a run reaching `done`/`failed`/`cancelled`) — i.e. a freed
  slot. Run *starts*, new queued runs, and `queued→running` transitions are
  intentionally ignored, so it won't wake you on non-events. It returns on the
  **first** completion — call it in a loop, one return per tick, and you (not
  the CLI) decide what to do with each freed slot. See the per-completion loop
  in the `orx-experiment-tree` skill.
- **It's a sleep-until-change signal, not the source of truth.** It reports
  only completions it saw *during that one call*; a run that finishes between
  calls is already terminal next time and won't be reported. On every return,
  re-read `orx runs <projectId>` and act on *all* newly-terminal runs — don't
  treat the printed line as the complete set, and don't replace `exp wait`
  with a tight `orx runs` poll either (use `exp wait` to sleep, `orx runs` to
  reconcile).
- Call `--project` **while runs are in flight** (right after launching). If
  every run is already terminal, there's nothing left to complete, so it
  returns immediately printing `drained: no runs in flight` (exit 0) — your
  loop's termination signal.
- `--interval` is seconds between polls (default `5`); `--timeout` gives up
  after N seconds (default `1800`) and exits **non-zero** so callers can branch
  on it. For long training runs, raise `--timeout` (or treat a timeout exit as
  "nothing changed yet, loop again") so a wait that simply outlasts the
  interval isn't mistaken for an error.
- Progress lines go to **stderr**; the final completion line(s) go to
  **stdout**, each as `<runId> <prev> -> <status>` (or `<runId> <status>
  (new)`), or the single line `drained: no runs in flight` when nothing was in
  flight.
- **When a run is `failed`, read `orx logs <runId>`** — the traceback, OOM, or
  setup error lives there (`orx runs` prints a failure reason under the table
  when one is known). Provider spin-up failures are usually transient and
  retryable: re-launch, or pick a different flavor or backend, rather than
  treating the experiment as a dead end.
- **A failed run is not a new node.** Re-launch the same `<expId>` — a failure
  answered nothing, so the node is still repairable in place
  (`orx-experiment-tree`).

## Sizing compute

- **Decide GPU vs CPU first.** API-driven evals, data prep, and CPU-bound
  papers run fine (and far cheaper) on a CPU flavor.
- **Pick the smallest flavor that fits** the model and a minimal batch; don't
  reflexively grab the biggest.
- **Let a real failure escalate you.** OOM or hopelessly-slow → move up a
  tier. That's expected, not a mistake.
- Raise `--timeout` (`--timeout 1d`) only for genuinely long runs.
