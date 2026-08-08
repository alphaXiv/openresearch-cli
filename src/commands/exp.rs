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
//! `crate::plane::{server_plane, local_plane}`. Only the two job-launch helpers
//! (`default_hf_image` / `spawn_detached_supervise`) stay
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

/// Persist cancel intent and ensure an orphaned run gets a fresh supervisor.
pub(crate) fn request_local_run_cancel(store: &Store, run_id: &str) -> Result<()> {
    let lock_path = crate::store::log_path(run_id).with_extension("cancel.lock");
    request_local_run_cancel_with(store, run_id, &lock_path, || {}, spawn_detached_supervise)
}

fn request_local_run_cancel_with(
    store: &Store,
    run_id: &str,
    lock_path: &std::path::Path,
    before_lock: impl FnOnce(),
    spawn: impl FnOnce(&str) -> Result<()>,
) -> Result<()> {
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path)?;
    let mut cancel_lock = fd_lock::RwLock::new(lock_file);
    before_lock();
    let _cancel_guard = cancel_lock.write()?;
    let prior = store
        .get_run(run_id)?
        .ok_or_else(|| anyhow!("Run {run_id} not found in the local store."))?
        .cancel_requested;
    store.set_cancel_requested(run_id, true)?;
    if let Err(spawn_err) = spawn(run_id) {
        if let Err(rollback_err) = store.set_cancel_requested(run_id, prior) {
            return Err(anyhow!(
                "Could not recover the supervisor: {spawn_err}; could not restore retryable cancel state: {rollback_err}"
            ));
        }
        return Err(spawn_err);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::StoredRun;

    fn run_fixture() -> StoredRun {
        StoredRun {
            id: "run-1".into(),
            experiment_id: "experiment-1".into(),
            project_id: "project-1".into(),
            status: "running".into(),
            backend_json: "{}".into(),
            command: String::new(),
            created_at: 1,
            updated_at: 1,
            ended_at: None,
            exit_code: None,
            commit_sha: None,
            result_markdown: None,
            cancel_requested: false,
            chat_session_id: None,
        }
    }

    #[test]
    fn failed_supervisor_spawn_restores_cancel_retry() {
        let dir =
            std::env::temp_dir().join(format!("orx-cancel-spawn-test-{}", uuid::Uuid::new_v4()));
        let store = Store::open_at(dir.clone()).unwrap();
        let run = run_fixture();
        store.upsert_run(&run).unwrap();
        let lock_path = dir.join("cancel.lock");

        let result = request_local_run_cancel_with(
            &store,
            &run.id,
            &lock_path,
            || {},
            |_| Err(anyhow!("synthetic spawn failure")),
        );
        assert!(result.is_err());
        assert!(!store.get_run(&run.id).unwrap().unwrap().cancel_requested);

        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_spawn_failure_preserves_successful_cancel_intent() {
        let dir = std::env::temp_dir().join(format!(
            "orx-cancel-concurrency-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = Store::open_at(dir.clone()).unwrap();
        let run = run_fixture();
        store.upsert_run(&run).unwrap();
        drop(store);
        let lock_path = dir.join("cancel.lock");
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (attempted_tx, attempted_rx) = std::sync::mpsc::channel();
        let (completed_tx, completed_rx) = std::sync::mpsc::channel();

        let first_dir = dir.clone();
        let first_lock = lock_path.clone();
        let first = std::thread::spawn(move || {
            let store = Store::open_at(first_dir).unwrap();
            request_local_run_cancel_with(
                &store,
                "run-1",
                &first_lock,
                || {},
                |_| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Err(anyhow!("synthetic spawn failure"))
                },
            )
            .unwrap_err();
        });
        entered_rx.recv().unwrap();

        let second_dir = dir.clone();
        let second_lock = lock_path.clone();
        let second = std::thread::spawn(move || {
            let store = Store::open_at(second_dir).unwrap();
            request_local_run_cancel_with(
                &store,
                "run-1",
                &second_lock,
                || attempted_tx.send(()).unwrap(),
                |_| Ok(()),
            )
            .unwrap();
            completed_tx.send(()).unwrap();
        });
        attempted_rx.recv().unwrap();
        let completed_while_locked = completed_rx
            .recv_timeout(std::time::Duration::from_millis(250))
            .is_ok();
        release_tx.send(()).unwrap();
        first.join().unwrap();
        second.join().unwrap();

        let store = Store::open_at(dir.clone()).unwrap();
        assert!(!completed_while_locked);
        assert!(store.get_run("run-1").unwrap().unwrap().cancel_requested);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
