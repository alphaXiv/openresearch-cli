//! The `exp` command group: operate on a single experiment node by id.
//!
//!   orx exp status <expId>            inspect status, run command, latest run
//!   orx exp cmd    <expId> [--set …]  view or set the run command
//!   orx exp run    <expId> …          launch a run on new or existing compute
//!   orx exp cancel <expId>            cancel the in-flight run
//!
//! Unlike the project-scoped data commands, every verb here takes an
//! *experiment* id (from `orx experiments <projectId>`).
//!
//! This module is now thin: it parses args and resolves the id to a
//! `ControlPlane`, then calls one verb. The per-plane bodies live in
//! `crate::plane::{server_plane, local_plane}`. Only the three job-launch helpers
//! (`hf_clone_script` / `default_hf_image` / `spawn_detached_supervise`) stay
//! here — every `src/local/*` backend imports them as `crate::commands::exp::*`.

use std::time::{Duration, Instant};

use crate::error::{anyhow, Result};
use crate::plane::{resolve_experiment, resolve_project};
use crate::store::Store;
use crate::ExpCommand;

pub async fn run(args: crate::ExpArgs) -> Result<()> {
    // Local-mode detection first: an id in `local_experiments` takes the local
    // path, and credentials are only required on the server path (a local-only
    // user may never have logged in). The plane resolver encodes that.
    let store = Store::open()?;
    match args.command {
        ExpCommand::Status { exp_id } => {
            resolve_experiment(store, &exp_id)?
                .experiment_status()
                .await
        }
        ExpCommand::Cmd { exp_id, set } => {
            resolve_experiment(store, &exp_id)?
                .set_experiment_command(set)
                .await
        }
        ExpCommand::Desc { exp_id, set, stdin } => {
            resolve_experiment(store, &exp_id)?
                .experiment_desc(set, stdin)
                .await
        }
        ExpCommand::Run(run_args) => {
            let run_args = *run_args;
            resolve_experiment(store, &run_args.exp_id)?
                .launch(run_args)
                .await
        }
        ExpCommand::Cancel { exp_id } => resolve_experiment(store, &exp_id)?.cancel().await,
        ExpCommand::Wait {
            exp_id,
            project,
            timeout,
            interval,
        } => wait(store, exp_id, project, timeout, interval).await,
    }
}

/// `orx exp wait …` — block on run state, for agents driving a research loop.
///
/// Two modes, picked by argument:
///   - `<expId>` — level trigger: poll the experiment's latest run until it reaches a terminal state (done/failed/cancelled).
///   - `--project` — edge trigger: snapshot every run in the project and return when the first run *completes* — i.e. transitions into a terminal state (done/failed/cancelled). This is the "a slot just freed" signal a budget-saturation loop wants; run starts and queued→running transitions are intentionally ignored.
///
/// Polls every `--interval` seconds (default 5), gives up after `--timeout`
/// seconds (default 1800) with a non-zero exit so callers can branch on it. The
/// per-plane polling loops are `ControlPlane::{wait_experiment, wait_project}`.
async fn wait(
    store: Store,
    exp_id: Option<String>,
    project: Option<String>,
    timeout: Option<u64>,
    interval: Option<u64>,
) -> Result<()> {
    let interval = Duration::from_secs(interval.unwrap_or(5).max(1));
    let deadline = Instant::now() + Duration::from_secs(timeout.unwrap_or(1800));

    match (exp_id, project) {
        (Some(_), Some(_)) => Err(anyhow!("Pass either <expId> or --project, not both.")),
        (None, None) => Err(anyhow!(
            "Specify what to wait on: `orx exp wait <expId>` (one run) or \
             `orx exp wait --project <projectId>` (any run in a project)."
        )),
        (Some(exp_id), None) => {
            resolve_experiment(store, &exp_id)?
                .wait_experiment(interval, deadline)
                .await
        }
        (None, Some(project_id)) => {
            resolve_project(store, &project_id)?
                .wait_project(interval, deadline)
                .await
        }
    }
}

// --- job-launch helpers shared with the src/local/* backends -----------------

/// Clone the experiment branch tip and run the fixed command. GITHUB_TOKEN
/// (passed as a job secret when present locally) authenticates private
/// repos via bash's ${VAR:+...} — the URL stays tokenless without it.
pub(crate) fn hf_clone_script(branch: &str, owner: &str, repo: &str, cmd: &str) -> String {
    format!(
        "set -eo pipefail; command -v git >/dev/null 2>&1 || (apt-get update -qq && apt-get install -y -qq git); \
         git clone --depth 1 --branch {branch} \"https://${{GITHUB_TOKEN:+x-access-token:${{GITHUB_TOKEN}}@}}github.com/{owner}/{repo}.git\" repo; \
         cd repo; {cmd}"
    )
}

/// Default docker image per flavor family: plain python for CPU flavors, a
/// CUDA-ready pytorch image for GPU flavors. Override with --image.
pub(crate) fn default_hf_image(flavor: &str) -> String {
    if flavor.starts_with("cpu") {
        "python:3.12".to_string()
    } else {
        "pytorch/pytorch:2.6.0-cuda12.4-cudnn9-runtime".to_string()
    }
}

/// Spawn `orx supervise <runId>` fully detached (own process group, no stdio),
/// so it outlives this command and any SSH session that launched it.
pub(crate) fn spawn_detached_supervise(run_id: &str) -> Result<()> {
    let exe = std::env::current_exe().map_err(|e| {
        anyhow!(
            "Could not locate the orx binary to spawn the supervisor: {}",
            e
        )
    })?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("supervise")
        .arg(run_id)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()
        .map_err(|e| anyhow!("Could not spawn `orx supervise {}`: {}", run_id, e))?;
    Ok(())
}
