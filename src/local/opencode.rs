//! opencode bootstrap for `orx up` — binary discovery, per-session config +
//! playbook written into the session's worktree, spawn + health check, and the
//! shared `AgentHost` handle the axum server holds (`Arc<AgentHost>` in state).
//!
//! One opencode serve child **per chat session**, cwd = that session's private
//! worktree (see `git::ensure_session_worktree`) so parallel agents never share
//! a checkout. Env is inherited (that's where ANTHROPIC_API_KEY /
//! OPENROUTER_API_KEY live — opencode auto-detects providers from env).
//! Children die with `orx up` via `kill_on_drop`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::json;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::error::{anyhow, Result};
use crate::local::git;
use crate::local::model::LocalProject;
use crate::store;

/// Playbook path inside the session worktree; opencode re-reads it every turn,
/// so rewriting the file retargets a running server without a restart.
const PLAYBOOK_REL: &str = ".openresearch/agent/autoresearch-local.md";

const HEALTH_TIMEOUT: Duration = Duration::from_secs(30);

/// `opencode` on PATH, else the installer's default drop location.
pub fn find_opencode() -> Result<PathBuf> {
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join("opencode");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    if let Some(home) = dirs::home_dir() {
        let fallback = home.join(".opencode").join("bin").join("opencode");
        if fallback.is_file() {
            return Ok(fallback);
        }
    }
    Err(anyhow!(
        "opencode not found (checked PATH and ~/.opencode/bin/opencode).\n\
         Install it with: curl -fsSL https://opencode.ai/install | bash"
    ))
}

/// Ask the OS for a free loopback port (bind :0, read it back, release).
fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| anyhow!("Could not pick a free port: {}", e))?;
    Ok(listener.local_addr()?.port())
}

/// Where the spawned server's stdout/stderr land (startup diagnostics).
pub fn agent_log_path() -> PathBuf {
    store::data_dir().join("agent-opencode.log")
}

/// Project-local opencode config. Every real permission is pre-approved so a
/// headless turn never stalls on a TUI prompt; the interactive `question` tool
/// is denied AND disabled (it would deadlock serve mode — nothing can answer
/// it), repeated on the default `build` agent because the tool filter is
/// agent-scoped. `model` only when the user passed `orx up --model`.
fn opencode_config_json(model: Option<&str>, instructions: &str) -> String {
    let mut cfg = json!({
        "$schema": "https://opencode.ai/config.json",
        "permission": {
            "edit": "allow",
            "bash": "allow",
            "webfetch": "allow",
            "websearch": "allow",
            "read": "allow",
            "glob": "allow",
            "grep": "allow",
            "task": "allow",
            "skill": "allow",
            "lsp": "allow",
            "doom_loop": "allow",
            "external_directory": "allow",
            "question": "deny",
        },
        "tools": { "question": false },
        "agent": {
            "build": { "tools": { "question": false }, "permission": { "question": "deny" } }
        },
        "instructions": [instructions],
    });
    if let Some(model) = model {
        cfg["model"] = json!(model);
    }
    serde_json::to_string_pretty(&cfg).unwrap_or_else(|_| "{}".to_string())
}

/// The local-mode autoresearch playbook: project context + cardinal rules +
/// the v1 local command surface. Ported from the cloud agent's
/// `autoresearchMd()`/`projectContextMd()` prompts, adapted for `orx up`
/// (external backends via `--backend`, analysis via `orx logs`, no
/// artifacts/query/chart).
fn playbook_md(project: &LocalProject) -> String {
    let id = &project.id;
    let name = &project.name;
    let repo = format!("{}/{}", project.github_owner, project.github_repo);
    let baseline = &project.baseline_branch;
    let files = super::files::files_dir(project)
        .to_string_lossy()
        .into_owned();
    let paper_line = project.paper_id.as_deref().map_or(String::new(), |p| {
        format!(
            "- Paper: arXiv {p} (https://arxiv.org/abs/{p}) — the paper this project starts \
             from; `orx paper {p}` fetches its report\n"
        )
    });
    format!(
        r#"# OpenResearch local agent — {name}

You are the OpenResearch research agent for the **local** project **{name}**,
running inside `orx up` on the user's own machine. Your working directory is
**your own git worktree** of the project's repo — private to this chat
session. Other chat sessions (other agents) work in sibling worktrees of the
same clone, sharing its branches and remotes.

- Project id: `{id}`
- GitHub repo: `{repo}`
- Baseline branch: `{baseline}`
{paper_line}- Compute: backends — `hf`, `modal`, `k8s`, `ssh`, `slurm`, or `local` —
  chosen by the user per run; **there is no default backend** (see "Compute
  backends")
- Files dir: `{files}` — every file in it shows up in the dashboard's
  Files tab (reports, figures, CSVs), grouped by experiment

## Start here

Drive everything through the `orx` CLI. `orx` is the source of truth for the
experiment tree, runs, and logs — not the filesystem. This is **local mode**:
only the commands listed below exist; use this project id (`{id}`) for every
`orx` command that takes one.

Orient with `orx projects` and `orx runs {id}`.

**If the experiment tree is empty** (a fresh project), create the baseline
first: `orx create-experiment {id} --title "Baseline"` (no `--parent`). Give it
the run command, run it once for reference numbers, then branch children off it.

## Working alongside other agents

Several chat sessions may drive this project at once, each in its own worktree
of the same clone. Git state is shared between you:

- **See their work before starting yours.** Local and remote branches are
  shared across worktrees — `git branch -a` lists every experiment branch
  (even unpushed ones), `orx runs {id}` shows what is running, and
  `orx exp desc <expId>` holds each node's findings. Orient from these so you
  extend the tree instead of duplicating a sibling's experiment.
- **Keep your notes current as you go.** Other agents orient from
  `orx exp desc` — write findings there when you learn them, not only at the
  end of a line of work.
- **One branch, one owner.** Git refuses to check out a branch that another
  worktree already has checked out. If `git checkout <branch>` fails that
  way, another agent owns that experiment — leave it alone and work on your
  own node.
- Your worktree starts **detached on the baseline tip**; check out your
  experiment's branch before editing.

## Cardinal rules

Breaking any of these silently invalidates results — they are not style
preferences.

1. **Never edit a baseline (root experiment) once it exists.** A root is the
   control its variants are measured against — on a fresh project you create
   it (first `orx create-experiment`, no `--parent`), and from then on it is
   frozen. To try an idea, **branch a child**
   (`orx create-experiment … --parent <expId>`) and edit the child's branch.
2. **The run command and the environment are a fixed contract — identical on
   every node.** Children inherit it verbatim. If the project has no run
   command, set the default once with `orx project edit {id} --run-command
   '<cmd>'` (or pass `--run-command` when creating the first experiment) —
   children inherit it from then on. Never vary behavior through env vars or
   env-prefixed commands.
3. **Vary code, not knobs-in-the-command.** Encode hyperparameters in committed
   code/config and branch a child per variant. Every node runs the *same*
   command over *different code*, so results stay comparable.
4. **Grow the tree downward, not sideways.** Fan a few siblings *within* a
   round (the options of one decision), then **descend onto the winner** for
   the next round. A root with a long flat row of children is the failure mode.
5. **Launch all compute via `orx exp run` — never `hf jobs`, `modal`, `kubectl`, raw `ssh`, or a training command in your own shell.** Direct jobs are unsupervised and invisible to the dashboard.

## Command surface (local mode)

| Command | What it does |
|---|---|
| `orx projects` | List projects; local ones are tagged `(local)`. |
| `orx create-experiment {id} --title "<t>" [--description "<d>"] [--parent <expId> \| --baseline] [--run-command "<cmd>"]` | New node on its own `orx/<slug>` branch, pushed to GitHub — forked off the parent's tip, or off `{baseline}` for a root (the base branch itself is never an experiment node). Omit `--parent`: attaches under the oldest project root — or, on an empty project, becomes the baseline root itself. `--baseline` forces another root (multiple baselines are allowed). |
| `orx project view {id}` / `orx project edit {id} --run-command "<cmd>"` | Inspect the project / set its default run command. |
| `orx exp status <expId>` | Node's branch, command, and latest run. |
| `orx exp desc <expId> [--set "<text>" \| --stdin]` | Read/overwrite the node's notes. Record findings here. |
| `orx exp run <expId> --backend <hf\|modal> --flavor <flavor> [--timeout 4h] [--image <img>]` | Launch the node's run on managed-SKU compute (see "Compute backends"). |
| `orx exp run <expId> --backend k8s [--manifest <path>] [--timeout 4h]` | Launch on the user's cluster from the manifest committed on the branch (default `.orx/k8s.yaml`). No flavors or --image — the manifest declares the resources. |
| `orx exp run <expId> --backend ssh --host <alias>` | Launch as a detached process on the user's own box (an `~/.ssh/config` alias). |
| `orx exp run <expId> --backend slurm [--host <alias>] [--flavor h100:2] [--timeout 4h]` | Launch as a batch job on the user's Slurm cluster (login node from `--host` or the slurm settings default; `--flavor` is a GRES GPU request, omit for CPU-only). |
| `orx exp run <expId> --backend openresearch --flavor <gpu_id[:count]\|cpu5c\|cpu5g\|cpu5m[:vcpus]> [--org <id>] [--disk GB] [--provider P] [--timeout 4h]` | Launch on an **ephemeral OpenResearch box** billed to the user's org (needs `orx login`) — provisioned for this run and deleted when it ends. GPU ids from `orx compute`. |
| `orx exp run <expId> --backend local` | Launch as a detached, supervised process on **this machine** (see "Where runs execute"). No flags — the hardware is whatever this machine has. |
| `orx exp cancel <expId>` | Cancel the in-flight run. |
| `orx exp wait <expId> [--timeout <s>]` / `orx exp wait --project {id}` | Poll until a run reaches a terminal state (project form returns on the first completion). Exits **non-zero** after `--timeout` seconds (default 1800) with nothing changed — that means "still running", not an error. |
| `orx runs {id} [--experiment <expId>]` | Run table, newest first. Run ids come from here. |
| `orx logs <runId> [--head] [--bytes <n>] [--range <s>:<e>]` | Read a run's log (tail by default). |

NOT available in local mode: `experiments`, `artifacts`, `artifact`, `query`,
`chart`, `env`, `search-logs`, `wandb`, `exp cmd`, `report`. Do not reach for
them — analysis happens through `orx logs`.

`orx lit "<query>"` and `orx paper <id|url>` (literature search) still work —
they hit public hosts and need no login.

## The auto-research loop

Carry one goal across many runs:

0. **Round 0 — the baseline** (empty project only): `orx create-experiment {id}
   --title "Baseline"` (no `--parent`), set the run command, launch it once.
   Its numbers are the reference every variant is judged against.
1. **Branch**: `orx create-experiment {id} --title "<idea>" --parent <parentId>`
   — one child per distinct thing you try.
2. **Edit** in this worktree: `git fetch origin && git checkout <branch>`,
   change the code, commit, and `git push`. The job clones from GitHub, so
   **unpushed work never runs**.
3. **Launch**: `orx exp run <expId> --backend <backend>` (`--flavor` for
   hf/modal, `--host` for ssh/slurm; k8s reads the committed manifest; local
   takes no flags).
4. **Wait — hold your turn open**: call `orx exp wait <expId> --timeout 480`
   (or `--project` when several are in flight) in a loop. Exit 0 → the run is
   terminal, go analyze. Non-zero → nothing changed yet; immediately call it
   again. Each call stays under your shell tool's own time limit.
5. **Analyze**: `orx logs <runId>` — read the metrics the run printed.
6. **Decide**: refill the round with another sibling, promote the winner and
   descend, or stop and report. Write what you learned into `orx exp desc`.

When a line of work concludes (or the user asks for a write-up), write a
report **directly into the files dir**. Its layout mirrors the experiment
tree — every top-level folder is named for an experiment slug:

- Per-experiment output goes in the folder named for its slug:
  `{files}/<experiment-slug>/report.md`, plus an `images/` subfolder for any
  figures it references by relative path. One experiment, one folder — its
  `report.md` is that experiment's findings.
- Cross-experiment syntheses and anything not tied to one node (comparisons,
  lit reviews) go under the reserved `project/` namespace as their own
  report folders: `{files}/project/<topic>/report.md`.

A report's first `# ` heading becomes its title. There is no upload step;
anything under `{files}` (reports, figures, data files) appears in the
dashboard's Files tab immediately, grouped by experiment.

When the user gives you a research task, see it through this loop — don't stop
after a single step or hand back a half-finished attempt. End your turn only
when the task is achieved, genuinely blocked on a decision only the user can
make, or the approach is exhausted. (For a plain question, just answer it.)

## Staying online while runs execute

Nothing re-invokes you when a run finishes, and there are no background
monitors — any process you background dies when your turn ends, so "I'll keep
watching the run" is not something you can do. While a run you launched is in
flight, the wait loop above IS your job: stay in it, and end your turn only
once you've read the result and acted on it. (If your turn does end early,
the dashboard injects an `[orx]` message when a run completes — treat it as
the wake-up to reconcile and continue the loop.)

## Referencing files

When you point the reader at a repo source file in chat, wrap it so they can
open it in the dashboard's file viewer: `<file path="relative/path.py" />`, or
with a line target `<file path="relative/path.py" lines="20-40" />`. Use
repo-relative paths (from the worktree root), not absolute paths. Reach for this
whenever you'd otherwise write a bare file path or a markdown link to a file —
the file you edited, the entrypoint you're describing, the config you changed.

## Where runs execute

**Never train or evaluate directly in your shell or worktree.** Your worktree
is the edit box: git, reading and writing code, and `orx` orchestration happen
here. The run itself — anything that trains, evaluates, or produces results —
always goes through `orx exp run`: a raw `python train.py` in your shell is
unsupervised, invisible to the dashboard, runs whatever happens to be in your
checkout instead of the branch tip, and blocks your turn. Lightweight
editor-side checks (`git`, `orx`, a quick `python -c "import x"`) are all that
belong in your shell.

Running **on this machine** is fine when the user picks it — that's
`--backend local`, which still goes through the run contract (clones the
branch tip into its own run dir, supervised, visible in the dashboard). It
shares CPU/RAM/GPU with the dashboard and your editing, so prefer it for
small or CPU-scale runs and use a remote backend for anything heavy.

## Compute backends

`orx exp run` requires an explicit `--backend` — **there is no default**.
Which backend to use is the user's decision: if the task doesn't name one and
the user hasn't already picked one in this conversation, ask before launching.
All of them share the same contract — the job clones the experiment branch's
GitHub tip and runs the fixed run command, and everything downstream
(`orx exp wait` / `orx runs` / `orx logs` / `orx exp cancel`) works
identically. A detached `orx supervise` mirrors status and logs; don't kill it.

| Backend | Runs on | Shape comes from |
|---|---|---|
| `hf` | Hugging Face Jobs — billed per minute to the user's HF account (`HF_TOKEN`) | `--flavor`: `cpu-basic` / `cpu-upgrade` (CPU-only), `t4-small`, `t4-medium`, `l4x1`, `l4x4`, `l40sx1`, `a10g-small`, `a10g-large`, `a100-large`, `h100`, `h200`, … |
| `modal` | Modal Sandboxes — billed per second to the user's Modal account (`MODAL_TOKEN_ID`/`MODAL_TOKEN_SECRET` or `~/.modal.toml`) | `--flavor`: a Modal GPU (`t4`, `l4`, `a10g`, `a100`, `a100-80gb`, `l40s`, `h100`, `h200`; append `:N` for a count, e.g. `h100:2`) or `cpu` / `cpu-large` |
| `k8s` | the user's own Kubernetes cluster — auth from their kubeconfig; context/namespace in Settings → Compute | a **manifest you commit on the experiment branch** (default `.orx/k8s.yaml`, or `--manifest <path>`) — see below |
| `ssh` | a detached process on the user's own box — no scheduler, no container, the host's environment as-is | `--host`: an `~/.ssh/config` host alias |
| `slurm` | a batch job on the user's Slurm cluster, submitted via `sbatch` on the login node over ssh | `--host`: the login node's `~/.ssh/config` alias (defaults from the slurm settings); `--flavor`: a GRES GPU request (`h100:2`; omit for CPU-only) |
| `openresearch` | an **ephemeral OpenResearch box** billed to the user's org (needs `orx login` + a registered SSH key) — provisioned for the run, deleted when it ends; fixed CUDA+PyTorch+uv image | `--flavor`: a GPU id from `orx compute` (`h100_sxm`, `h100_sxm:2`) or a CPU flavor (`cpu5c`/`cpu5g`/`cpu5m`, `cpu5c:32`); plus `[--org <id>] [--disk GB] [--provider P]` |
| `local` | a detached process on **this machine** — no scheduler, no container, this machine's environment as-is | nothing — no flags; the hardware is whatever this machine has |

- `--timeout` (default `4h`) applies to `hf`/`modal`/`k8s`/`slurm`/
  `openresearch`; set it to cover the whole run — a job killed at the timeout
  reads as a failed run. Doesn't apply to `ssh` or `local` (the process runs
  until it exits or is cancelled). On k8s a manifest-set
  `activeDeadlineSeconds` wins over the flag.
- `--image` overrides the container on `hf`/`modal` (default: CUDA pytorch on
  GPU flavors, `python:3.12` on CPU). Doesn't apply to `ssh`/`slurm`/`local`
  (the host's own environment), `k8s` (the manifest sets the image), or
  `openresearch` (the platform's fixed image).

### The k8s manifest contract

There are no flavors or topology flags: **you write plain Kubernetes YAML**,
commit it on the experiment branch, and orx applies it. Inspect the cluster
yourself (`kubectl get nodes`, allocatable resources, GPU products) and write
whatever the run needs — a single-pod 4-GPU Job, an Indexed Job spanning
nodes with a headless Service and downward-API rank env, an auxiliary
inference Deployment. The manifest inherits through the tree like all code,
and changing it is a commit — visible in the diff like any experimental
variable.

Rules orx enforces at submit (loud, before anything runs):

- **Exactly one Job** — its completion/failure is the run's outcome. With
  several Jobs, label the primary `orx-primary: "true"`.
- **Some container of that Job must run the injected script**: set
  `command: ["bash", "-c", "$ORX_SCRIPT"]`. The `ORX_SCRIPT` env var (added
  by orx) clones the branch tip and runs the experiment's fixed run command —
  the run command stays the contract; the manifest only shapes where it runs.
- Every resource needs `metadata.name` (no `generateName`) and no foreign
  `metadata.namespace`. Put `{{{{ORX_RUN}}}}` in names — orx substitutes a
  run-unique token so re-runs don't collide.

orx injects the rest: run labels, the `orx-env` Secret (`envFrom`, holds the
synced API keys + `HF_TOKEN`/`GITHUB_TOKEN`) on the primary Job, and defaults
for `activeDeadlineSeconds`/`ttlSecondsAfterFinished`/`backoffLimit: 0` when
unset. Auxiliary resources that need the env reference the `orx-env` Secret
themselves. Cancel deletes exactly what the manifest created.

The run log follows the primary Job's **leader pod** (completion index 0 for
Indexed Jobs, else its sole pod) — print everything you'll need to analyze
from there; other pods stay reachable via `kubectl logs`. Cross-node traffic
rides the pod network — fine for loosely-coupled work (async RL,
parameter-server); tightly-coupled per-step all-reduce wants a fast fabric
the cluster may not have.

## Sizing compute

- **Decide GPU vs CPU first.** API-driven evals, data prep, and CPU-bound
  papers run fine (and far cheaper) on a CPU flavor.
- **Pick the smallest flavor that fits** the model and a minimal batch; don't
  reflexively grab the biggest.
- **Let a real failure escalate you.** OOM or hopelessly-slow → move up a
  tier. That's expected, not a mistake.
- Raise `--timeout` (`--timeout 1d`) only for genuinely long runs.

## Analyzing results

Run logs are the only evidence channel in local mode. Make the run command
print everything you'll need to stdout — final metrics, an `EVAL.md`-style
summary, key config — and read it back with `orx logs <runId>` (use `--head` /
`--range` for long logs). If a run's output isn't in its log, it's lost.

## Asking the user

Interactive prompt tools surface as cards in the chat UI — they do not hang.
If your harness provides a question tool (e.g. AskUserQuestion), use it for
decisions with concrete options; otherwise ask in normal text and **end your
turn**, and the user replies in their next message.

**Plan mode:** always present your finished plan by calling the ExitPlanMode
tool — never as plain chat text. The plan card is how the user approves the
plan and unlocks execution; a plan left in chat text strands the session in
plan mode.
"#
    )
}

/// Keep the files we drop into the checkout out of `git status` / accidental
/// commits via the local-only `.git/info/exclude` (never touches tracked
/// files or the repo's own `.gitignore`). Takes the **hub clone** path — its
/// `.git/info/exclude` is shared by every session worktree (a worktree's own
/// `.git` is just a pointer file). Best-effort.
fn exclude_agent_files(hub: &Path) {
    let path = hub.join(".git").join("info").join("exclude");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let missing: Vec<&str> = ["opencode.json", ".openresearch/"]
        .into_iter()
        .filter(|entry| !existing.lines().any(|l| l.trim() == *entry))
        .collect();
    if missing.is_empty() {
        return;
    }
    let mut block = String::new();
    if !existing.is_empty() && !existing.ends_with('\n') {
        block.push('\n');
    }
    for entry in missing {
        block.push_str(entry);
        block.push('\n');
    }
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, block.as_bytes()));
}

/// Ensure the project's hub clone and this session's private worktree exist,
/// and write the autoresearch playbook into the worktree. Every harness
/// adapter injects this same file (opencode via config `instructions`, Claude
/// Code via `--append-system-prompt`, Codex via `developerInstructions` —
/// legacy exec: first-turn context). Returns
/// `(workdir, playbook)` — the worktree the harness runs in and the playbook
/// path inside it.
pub fn ensure_playbook(project: &LocalProject, session_id: &str) -> Result<(PathBuf, PathBuf)> {
    let workdir = git::ensure_session_worktree(
        &project.github_owner,
        &project.github_repo,
        &project.baseline_branch,
        session_id,
    )?;
    let playbook = workdir.join(PLAYBOOK_REL);
    if let Some(parent) = playbook.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow!("Could not create {}: {}", parent.display(), e))?;
    }
    std::fs::write(&playbook, playbook_md(project))
        .map_err(|e| anyhow!("Could not write {}: {}", playbook.display(), e))?;
    // One shared exclude covers every worktree.
    exclude_agent_files(&git::clone_path(
        &project.github_owner,
        &project.github_repo,
    ));
    // The playbook points the agent at the files dir — make sure it exists.
    let _ = super::files::ensure_dir(project);
    Ok((workdir, playbook))
}

/// Write the opencode config + the playbook into the session's worktree
/// (self-healing via `ensure_session_worktree` if the cache was wiped).
/// Returns the worktree path plus, when the repo tracks its own
/// `opencode.json` (which we must never clobber — the agent commits and
/// pushes from this worktree), the path of our config to pass via
/// `OPENCODE_CONFIG` instead.
fn write_agent_files(
    project: &LocalProject,
    model: Option<&str>,
    session_id: &str,
) -> Result<(PathBuf, Option<PathBuf>)> {
    let (repo, playbook) = ensure_playbook(project, session_id)?;
    let config_override = if git::is_tracked(&repo, "opencode.json") {
        // Out-of-root config: absolute instructions path (no root to anchor it).
        let path = repo
            .join(".openresearch")
            .join("agent")
            .join("opencode.json");
        std::fs::write(
            &path,
            opencode_config_json(model, &playbook.to_string_lossy()),
        )
        .map_err(|e| anyhow!("Could not write {}: {}", path.display(), e))?;
        Some(path)
    } else {
        std::fs::write(
            repo.join("opencode.json"),
            opencode_config_json(model, PLAYBOOK_REL),
        )
        .map_err(|e| anyhow!("Could not write opencode.json: {}", e))?;
        None
    };
    Ok((repo, config_override))
}

/// Wire status of one serve child for `GET /api/agent/status`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentStatus {
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

struct AgentChild {
    child: Child,
    port: u16,
    project_id: String,
    session_id: String,
    model: Option<String>,
}

impl AgentChild {
    fn status(&self) -> AgentStatus {
        AgentStatus {
            running: true,
            port: Some(self.port),
            project_id: Some(self.project_id.clone()),
            session_id: Some(self.session_id.clone()),
            model: self.model.clone(),
        }
    }
}

/// Poll `/global/health` until opencode answers, watching for early exit.
async fn wait_healthy(child: &mut Child, port: u16) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?;
    let url = format!("http://127.0.0.1:{port}/global/health");
    let deadline = Instant::now() + HEALTH_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            return Err(anyhow!(
                "opencode exited during startup ({status}); see {}",
                agent_log_path().display()
            ));
        }
        if let Ok(resp) = client.get(&url).send().await {
            if resp.status().is_success() {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "opencode did not become healthy on 127.0.0.1:{port} within {}s; see {}",
                HEALTH_TIMEOUT.as_secs(),
                agent_log_path().display()
            ));
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}

/// Spawn `opencode serve` in the session's worktree and wait for it to come
/// up healthy.
async fn spawn_agent(
    project: &LocalProject,
    model: Option<&str>,
    session_id: &str,
) -> Result<AgentChild> {
    let bin = find_opencode()?;
    // The clone/worktree setup inside can hit the network; keep it off the
    // async workers.
    let (repo, config_override) = {
        let (project, model) = (project.clone(), model.map(str::to_string));
        let session = session_id.to_string();
        tokio::task::spawn_blocking(move || write_agent_files(&project, model.as_deref(), &session))
            .await
            .map_err(|e| anyhow!("agent file task failed: {e}"))??
    };
    // Best-effort: the playbook is the real guide; the shim just lets
    // opencode's skill tool surface `orx skill` too.
    if let Err(err) = crate::commands::install_skills::install_opencode_shim().await {
        eprintln!("warning: could not install the orx opencode skill: {err}");
    }
    let port = free_port()?;
    // The data dir may not exist yet (fresh machine, no Store::open before us).
    if let Some(parent) = agent_log_path().parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow!("Could not create {}: {}", parent.display(), e))?;
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(agent_log_path())
        .map_err(|e| anyhow!("Could not open {}: {}", agent_log_path().display(), e))?;

    let mut cmd = Command::new(&bin);
    cmd.arg("serve")
        .arg("--port")
        .arg(port.to_string())
        .arg("--hostname")
        .arg("127.0.0.1")
        // Without --print-logs the log file stays empty and startup failures
        // are undiagnosable.
        .arg("--print-logs")
        .current_dir(&repo)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone().map_err(|e| anyhow!("{e}"))?))
        .stderr(Stdio::from(log))
        // Dies with `orx up` when the runtime drops the handle (Ctrl-C, exit).
        .kill_on_drop(true);
    // The agent shells out to plain `orx`; prepend this binary's dir so it
    // resolves to THIS orx (with local mode), not an older install on PATH.
    if let Ok(exe) = std::env::current_exe().and_then(|p| p.canonicalize()) {
        if let Some(dir) = exe.parent() {
            let mut path = std::ffi::OsString::from(dir);
            match std::env::var_os("PATH") {
                Some(existing) if !existing.is_empty() => {
                    path.push(":");
                    path.push(existing);
                }
                _ => {}
            }
            cmd.env("PATH", path);
        }
    }
    // Vars saved in the dashboard's Environment tab reach the agent too;
    // the real process env still wins on conflicts.
    for (key, value) in crate::config::list_synced_env() {
        if std::env::var_os(&key).is_none() {
            cmd.env(key, value);
        }
    }
    if let Some(config) = &config_override {
        // The repo tracks its own opencode.json; ours rides OPENCODE_CONFIG.
        // Project configs load after OPENCODE_CONFIG and would override our
        // headless permission grants, so they are disabled for this child.
        cmd.env("OPENCODE_CONFIG", config)
            .env("OPENCODE_DISABLE_PROJECT_CONFIG", "1");
    }
    // Own process group: a terminal SIGINT reaches orx up alone, which then
    // tears the child down deliberately (kill_on_drop / shutdown()).
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("Could not spawn {}: {}", bin.display(), e))?;
    if let Err(err) = wait_healthy(&mut child, port).await {
        let _ = child.kill().await;
        return Err(err);
    }
    Ok(AgentChild {
        child,
        port,
        project_id: project.id.clone(),
        session_id: session_id.to_string(),
        model: model.map(str::to_string),
    })
}

/// The `orx up` opencode host: one serve child per chat session, keyed by the
/// orx session id, each running in that session's worktree. Share as
/// `Arc<AgentHost>` in axum state.
pub struct AgentHost {
    /// `orx up --model` override, applied to every spawn.
    model_override: Option<String>,
    /// Serializes ensure() spawns (across all sessions — a spawn is seconds,
    /// and one at a time keeps clone/fetch traffic sane). Never taken by
    /// status()/port_for(), and `inner` is never held across a spawn — a slow
    /// clone or health poll must not block status reads or turn replies.
    spawn_lock: Mutex<()>,
    inner: Mutex<HashMap<String, AgentChild>>,
}

impl AgentHost {
    pub fn new(model_override: Option<String>) -> Self {
        Self {
            model_override,
            spawn_lock: Mutex::new(()),
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Status of every live child; reaps children that died behind our back.
    pub async fn status(&self) -> Vec<AgentStatus> {
        let mut guard = self.inner.lock().await;
        guard.retain(|_, agent| matches!(agent.child.try_wait(), Ok(None)));
        guard.values().map(AgentChild::status).collect()
    }

    /// Loopback port of the session's live server (for inline replies/aborts).
    pub async fn port_for(&self, session_id: &str) -> Option<u16> {
        let mut guard = self.inner.lock().await;
        let agent = guard.get_mut(session_id)?;
        if matches!(agent.child.try_wait(), Ok(None)) {
            Some(agent.port)
        } else {
            guard.remove(session_id);
            None
        }
    }

    /// Spawn (or reuse) the opencode server for this session. Idempotent when
    /// the session's server is already alive; a dead child is replaced.
    pub async fn ensure(&self, project: &LocalProject, session_id: &str) -> Result<AgentStatus> {
        let _spawning = self.spawn_lock.lock().await;
        {
            let mut guard = self.inner.lock().await;
            if let Some(agent) = guard.get_mut(session_id) {
                if agent.project_id == project.id && matches!(agent.child.try_wait(), Ok(None)) {
                    return Ok(agent.status());
                }
            }
            if let Some(mut old) = guard.remove(session_id) {
                let _ = old.child.kill().await; // kill() also reaps
            }
        }
        // inner released: status()/port reads keep answering while the spawn
        // (clone/fetch + health poll) is in flight instead of hanging.
        let agent = spawn_agent(project, self.model_override.as_deref(), session_id).await?;
        let status = agent.status();
        self.inner
            .lock()
            .await
            .insert(session_id.to_string(), agent);
        Ok(status)
    }

    /// Kill and reap one session's child (on session delete). No-op when the
    /// session has none.
    pub async fn kill_session(&self, session_id: &str) {
        if let Some(mut agent) = self.inner.lock().await.remove(session_id) {
            let _ = agent.child.kill().await;
        }
    }

    /// Kill and reap every child (also happens via kill_on_drop on exit).
    pub async fn shutdown(&self) {
        for (_, mut agent) in self.inner.lock().await.drain() {
            let _ = agent.child.kill().await;
        }
    }
}
