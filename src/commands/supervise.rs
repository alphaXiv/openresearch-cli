//! `orx supervise <runId>` — the lens beside an external job.
//!
//! Spawned detached by `orx exp run --backend hf`; restart-idempotent (state
//! is the local store + the backend itself, and log dedup resumes from the
//! store's log file). Two concurrent halves: a tail task streams backend logs
//! into the run's log file, while the main loop polls job state, mirrors
//! transitions to the api (whose PATCH response carries cancel intent), and on
//! terminal status uploads the log via presigned PUT.
//!
//! API unreachability never kills supervision: the local store stays correct
//! and mirroring resumes on the next transition.
//!
//! Local-mode runs (`orx up`, experiment in `local_experiments`) skip the api
//! entirely: no credentials, no mirror, no upload — cancel intent comes from
//! the local run row's `cancel_requested` flag instead.

use std::io::Write as _;
use std::time::Duration;

use serde_json::json;

use crate::client::{presign_external_run_log, update_external_run, upload_to_presigned};
use crate::config::Credentials;
use crate::error::{anyhow, require_credentials, Result};
use crate::jobs::huggingface as hf;
use crate::jobs::kubernetes as k8s;
use crate::jobs::localbox;
use crate::jobs::modal;
use crate::jobs::openresearch;
use crate::jobs::slurm;
use crate::jobs::ssh;
use crate::jobs::{is_terminal_stage, stage_to_run_status, BackendDescriptor};
use crate::store::{log_path, now_ms, Store};

const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// How long a silent log stream is held before re-checking job state.
const LOG_IDLE: Duration = Duration::from_secs(30);

pub async fn run(args: crate::SuperviseArgs) -> Result<()> {
    let run_id = args.run_id;

    let store = Store::open()?;
    let stored = store
        .get_run(&run_id)?
        .ok_or_else(|| anyhow!("Run {} not found in the local store.", run_id))?;
    // Local runs never touch client.rs; credentials load only on the server path.
    let local = store.get_local_experiment(&stored.experiment_id)?.is_some();
    let creds = if local {
        None
    } else {
        Some(require_credentials().await)
    };
    let descriptor = BackendDescriptor::parse(&stored.backend_json)?;
    if descriptor.kind == "k8s_job" {
        return run_k8s(store, stored, descriptor, creds, run_id).await;
    }
    if descriptor.kind == "modal_job" {
        return run_modal(store, stored, descriptor, creds, run_id).await;
    }
    if descriptor.kind == "ssh_job" {
        return run_ssh(store, stored, descriptor, creds, run_id).await;
    }
    if descriptor.kind == "slurm_job" {
        return run_slurm(store, stored, descriptor, creds, run_id).await;
    }
    if descriptor.kind == "openresearch_job" {
        return run_openresearch(store, stored, descriptor, creds, run_id).await;
    }
    if descriptor.kind == "local_job" {
        return run_local(store, stored, descriptor, creds, run_id).await;
    }
    let (namespace, job_id) = descriptor.hf_ref()?;
    let namespace = namespace.to_string();
    let job_id = job_id.to_string();
    let token = hf::resolve_token()?;

    eprintln!("supervise {run_id}: watching hf job {namespace}/{job_id}");

    // Log tailing runs CONCURRENTLY with status polling — never in series.
    // `stream_logs` blocks for as long as the job keeps printing, so a
    // sequential loop would sit inside the stream until the job ended and only
    // then report `running`… as `done` (the UI would see no live run at all,
    // then the whole log at once). The tail task owns the log file; this loop
    // owns status, mirroring, and cancel intent.
    let path = log_path(&run_id);
    let (done_tx, done_rx) = tokio::sync::watch::channel(false);
    let mut log_task = tokio::spawn(tail_logs(
        token.clone(),
        namespace.clone(),
        job_id.clone(),
        path.clone(),
        run_id.clone(),
        done_rx,
    ));

    let mut last_status = stored.status.clone();
    let mut cancel_sent = false;

    loop {
        // Where is the job now?
        let job = match hf::inspect_job(&token, &namespace, &job_id).await {
            Ok(j) => j,
            Err(err) => {
                eprintln!("supervise {run_id}: inspect failed (will retry): {err}");
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
        };
        let stage = job.status.stage.as_str();
        let status = stage_to_run_status(stage).to_string();

        // Terminal: let the tail drain the stream's remainder, then persist
        // everything BEFORE reporting the final status, so the moment the UI
        // sees done/failed the R2 log already exists (the run page switches
        // from live tail to the persisted log on that flip).
        if is_terminal_stage(stage) {
            store.update_status(&run_id, &status, Some(now_ms()), None)?;
            // Local runs record the failure reason on the row itself — that's
            // what `orx logs`-adjacent surfaces (exp status, runs) read.
            if creds.is_none() && status == "failed" {
                if let Some(msg) = &job.status.message {
                    if let Err(err) =
                        store.set_result_markdown(&run_id, &format!("Job failed: {msg}"))
                    {
                        eprintln!("supervise {run_id}: could not record failure reason: {err}");
                    }
                }
            }
            let _ = done_tx.send(true);
            if tokio::time::timeout(Duration::from_secs(20), &mut log_task)
                .await
                .is_err()
            {
                log_task.abort();
            }
            if let Some(creds) = &creds {
                if let Ok(bytes) = std::fs::read(&path) {
                    if !bytes.is_empty() {
                        match presign_external_run_log(creds, &run_id).await {
                            Ok(presigned) => {
                                if let Err(err) = upload_to_presigned(
                                    &presigned.url,
                                    "application/octet-stream",
                                    bytes,
                                )
                                .await
                                {
                                    eprintln!("supervise {run_id}: log upload failed: {err}");
                                }
                            }
                            Err(err) => eprintln!("supervise {run_id}: log presign failed: {err}"),
                        }
                    }
                }
                if let Err(err) = mirror_status(creds, &run_id, &status, &job.status.message).await
                {
                    eprintln!("supervise {run_id}: final status mirror failed: {err}");
                }
            }
            eprintln!("supervise {run_id}: finished ({status})");
            return Ok(());
        }

        // Mirror a live transition (local store first — it's the truth).
        if status != last_status {
            store.update_status(&run_id, &status, None, None)?;
            let cancel_requested = match &creds {
                Some(creds) => mirror_status(creds, &run_id, &status, &job.status.message)
                    .await
                    .unwrap_or(false),
                None => local_cancel_requested(&store, &run_id),
            };
            eprintln!("supervise {run_id}: {last_status} -> {status} (stage {stage})");
            last_status = status.clone();
            if cancel_requested && !cancel_sent {
                request_backend_cancel(&token, &namespace, &job_id, &run_id, &mut cancel_sent)
                    .await;
            }
        } else if !cancel_sent {
            // No transition to report — poll cancel intent cheaply instead.
            let cancel_requested = match &creds {
                Some(creds) => crate::client::get_external_run_state(creds, &run_id)
                    .await
                    .map(|s| s.cancel_requested)
                    .unwrap_or(false),
                None => local_cancel_requested(&store, &run_id),
            };
            if cancel_requested {
                request_backend_cancel(&token, &namespace, &job_id, &run_id, &mut cancel_sent)
                    .await;
            }
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Local cancel intent from the run row itself. Best-effort — a transient
/// db error must not kill supervision.
fn local_cancel_requested(store: &Store, run_id: &str) -> bool {
    store
        .get_run(run_id)
        .ok()
        .flatten()
        .map(|r| r.cancel_requested)
        .unwrap_or(false)
}

/// PATCH the mirror; returns the server's cancel intent. Best-effort.
async fn mirror_status(
    creds: &Credentials,
    run_id: &str,
    status: &str,
    message: &Option<String>,
) -> Result<bool> {
    // The mirror never accepts "starting" (that's the registration state).
    if status == "starting" {
        return Ok(false);
    }
    let mut body = json!({ "status": status });
    if status == "failed" {
        if let Some(msg) = message {
            body["resultMarkdown"] = json!(format!("Job failed: {msg}"));
        }
    }
    let patched = update_external_run(creds, run_id, body).await?;
    Ok(patched.cancel_requested)
}

/// Tail the job's log stream into the run's log file until told we're done.
/// Reconnects forever (HF replays from the start; `seen` dedups), so a network
/// blip or the stream's own idle-close never loses the tail. Truncates on
/// open: a restarted supervisor rewrites the file from event zero rather than
/// appending a duplicate history.
async fn tail_logs(
    token: String,
    namespace: String,
    job_id: String,
    path: std::path::PathBuf,
    run_id: String,
    done: tokio::sync::watch::Receiver<bool>,
) {
    let mut log_file = match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(err) => {
            eprintln!(
                "supervise {run_id}: could not open {}: {err}",
                path.display()
            );
            return;
        }
    };
    let mut seen = 0u64;
    loop {
        let mut sink = |line: &str| {
            let _ = writeln!(log_file, "{line}");
        };
        match hf::stream_logs(&token, &namespace, &job_id, seen, LOG_IDLE, &mut sink).await {
            Ok(s) => seen = s,
            Err(err) => eprintln!("supervise {run_id}: log stream error (will retry): {err}"),
        }
        let _ = log_file.flush();
        // Between passes: exit once the job is terminal (the closed stream has
        // been fully drained by the pass above); otherwise breathe and retry.
        if *done.borrow() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn request_backend_cancel(
    token: &str,
    namespace: &str,
    job_id: &str,
    run_id: &str,
    cancel_sent: &mut bool,
) {
    eprintln!("supervise {run_id}: cancel requested — cancelling hf job");
    match hf::cancel_job(token, namespace, job_id).await {
        Ok(()) => *cancel_sent = true,
        Err(err) => eprintln!("supervise {run_id}: hf cancel failed (will retry): {err}"),
    }
}

// --- kubernetes ---------------------------------------------------------------
//
// Same two-half shape as the HF path (concurrent log tail + status poll), with
// kubectl as the transport. Cancel = delete the Job; the next inspect sees
// NotFound (stage DELETED) and the run lands on "cancelled".

async fn run_k8s(
    store: Store,
    stored: crate::store::StoredRun,
    descriptor: BackendDescriptor,
    creds: Option<Credentials>,
    run_id: String,
) -> Result<()> {
    let (namespace, job_name) = descriptor.k8s_ref()?;
    let namespace = namespace.to_string();
    let job_name = job_name.to_string();
    let context = descriptor.context.clone();
    // What cancel deletes: the manifest's recorded resources, or just the Job
    // for runs from before resource recording existed.
    let resources = descriptor
        .resources
        .clone()
        .unwrap_or_else(|| vec![format!("job/{job_name}")]);

    eprintln!("supervise {run_id}: watching k8s job {namespace}/{job_name}");

    let path = log_path(&run_id);
    let (done_tx, done_rx) = tokio::sync::watch::channel(false);
    let mut log_task = tokio::spawn(tail_logs_k8s(
        context.clone(),
        namespace.clone(),
        job_name.clone(),
        path.clone(),
        run_id.clone(),
        done_rx,
    ));

    let mut last_status = stored.status.clone();
    let mut cancel_sent = false;

    loop {
        let job = match k8s::inspect_job(context.as_deref(), &namespace, &job_name).await {
            Ok(j) => j,
            Err(err) => {
                eprintln!("supervise {run_id}: inspect failed (will retry): {err}");
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
        };
        let stage = job.stage.as_str();
        let status = stage_to_run_status(stage).to_string();

        if is_terminal_stage(stage) {
            store.update_status(&run_id, &status, Some(now_ms()), None)?;
            if creds.is_none() && status == "failed" {
                if let Some(msg) = &job.message {
                    if let Err(err) =
                        store.set_result_markdown(&run_id, &format!("Job failed: {msg}"))
                    {
                        eprintln!("supervise {run_id}: could not record failure reason: {err}");
                    }
                }
            }
            let _ = done_tx.send(true);
            if tokio::time::timeout(Duration::from_secs(20), &mut log_task)
                .await
                .is_err()
            {
                log_task.abort();
            }
            if let Some(creds) = &creds {
                if let Ok(bytes) = std::fs::read(&path) {
                    if !bytes.is_empty() {
                        match presign_external_run_log(creds, &run_id).await {
                            Ok(presigned) => {
                                if let Err(err) = upload_to_presigned(
                                    &presigned.url,
                                    "application/octet-stream",
                                    bytes,
                                )
                                .await
                                {
                                    eprintln!("supervise {run_id}: log upload failed: {err}");
                                }
                            }
                            Err(err) => eprintln!("supervise {run_id}: log presign failed: {err}"),
                        }
                    }
                }
                if let Err(err) = mirror_status(creds, &run_id, &status, &job.message).await {
                    eprintln!("supervise {run_id}: final status mirror failed: {err}");
                }
            }
            eprintln!("supervise {run_id}: finished ({status})");
            return Ok(());
        }

        if status != last_status {
            store.update_status(&run_id, &status, None, None)?;
            let cancel_requested = match &creds {
                Some(creds) => mirror_status(creds, &run_id, &status, &job.message)
                    .await
                    .unwrap_or(false),
                None => local_cancel_requested(&store, &run_id),
            };
            eprintln!("supervise {run_id}: {last_status} -> {status} (stage {stage})");
            last_status = status.clone();
            if cancel_requested && !cancel_sent {
                cancel_k8s(
                    context.as_deref(),
                    &namespace,
                    &resources,
                    &run_id,
                    &mut cancel_sent,
                )
                .await;
            }
        } else if !cancel_sent {
            let cancel_requested = match &creds {
                Some(creds) => crate::client::get_external_run_state(creds, &run_id)
                    .await
                    .map(|s| s.cancel_requested)
                    .unwrap_or(false),
                None => local_cancel_requested(&store, &run_id),
            };
            if cancel_requested {
                cancel_k8s(
                    context.as_deref(),
                    &namespace,
                    &resources,
                    &run_id,
                    &mut cancel_sent,
                )
                .await;
            }
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// k8s twin of `tail_logs` — `kubectl logs -f` replays from the pod's start on
/// each reconnect, so the same truncate-and-dedup contract applies. Tails the
/// primary Job's leader pod (index 0 for Indexed jobs).
async fn tail_logs_k8s(
    context: Option<String>,
    namespace: String,
    job_name: String,
    path: std::path::PathBuf,
    run_id: String,
    done: tokio::sync::watch::Receiver<bool>,
) {
    let mut log_file = match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(err) => {
            eprintln!(
                "supervise {run_id}: could not open {}: {err}",
                path.display()
            );
            return;
        }
    };
    let mut seen = 0u64;
    loop {
        let mut sink = |line: &str| {
            let _ = writeln!(log_file, "{line}");
        };
        match k8s::stream_logs(
            context.as_deref(),
            &namespace,
            &job_name,
            seen,
            LOG_IDLE,
            &mut sink,
        )
        .await
        {
            Ok(s) => seen = s,
            Err(err) => eprintln!("supervise {run_id}: log stream error (will retry): {err}"),
        }
        let _ = log_file.flush();
        if *done.borrow() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn cancel_k8s(
    context: Option<&str>,
    namespace: &str,
    resources: &[String],
    run_id: &str,
    cancel_sent: &mut bool,
) {
    eprintln!("supervise {run_id}: cancel requested — deleting the run's k8s resources");
    match k8s::delete_resources(context, namespace, resources).await {
        Ok(()) => *cancel_sent = true,
        Err(err) => eprintln!("supervise {run_id}: k8s cancel failed (will retry): {err}"),
    }
}

// --- modal --------------------------------------------------------------------
//
// Same two-half shape as the HF/k8s paths (concurrent log tail + status poll),
// with the Modal Python launcher as the transport. Cancel = terminate the
// sandbox; a terminated sandbox polls as a non-zero exit (ERROR), so once a
// cancel has been sent we report the terminal state as `cancelled` rather than
// `failed`.

async fn run_modal(
    store: Store,
    stored: crate::store::StoredRun,
    descriptor: BackendDescriptor,
    creds: Option<Credentials>,
    run_id: String,
) -> Result<()> {
    let sandbox_id = descriptor.modal_ref()?.to_string();

    eprintln!("supervise {run_id}: watching modal sandbox {sandbox_id}");

    let path = log_path(&run_id);
    let (done_tx, done_rx) = tokio::sync::watch::channel(false);
    let mut log_task = tokio::spawn(tail_logs_modal(
        sandbox_id.clone(),
        path.clone(),
        run_id.clone(),
        done_rx,
    ));

    let mut last_status = stored.status.clone();
    let mut cancel_sent = false;

    loop {
        let job = match modal::inspect_job(&sandbox_id).await {
            Ok(j) => j,
            Err(err) => {
                eprintln!("supervise {run_id}: inspect failed (will retry): {err}");
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
        };
        let stage = job.stage.as_str();
        // A terminated sandbox reports a non-zero exit; if we asked for the
        // cancel, that terminal state is a cancellation, not a failure.
        let status = if cancel_sent && is_terminal_stage(stage) {
            "cancelled".to_string()
        } else {
            stage_to_run_status(stage).to_string()
        };

        if is_terminal_stage(stage) {
            store.update_status(&run_id, &status, Some(now_ms()), None)?;
            if creds.is_none() && status == "failed" {
                if let Some(msg) = &job.message {
                    if let Err(err) =
                        store.set_result_markdown(&run_id, &format!("Job failed: {msg}"))
                    {
                        eprintln!("supervise {run_id}: could not record failure reason: {err}");
                    }
                }
            }
            let _ = done_tx.send(true);
            if tokio::time::timeout(Duration::from_secs(20), &mut log_task)
                .await
                .is_err()
            {
                log_task.abort();
            }
            if let Some(creds) = &creds {
                if let Ok(bytes) = std::fs::read(&path) {
                    if !bytes.is_empty() {
                        match presign_external_run_log(creds, &run_id).await {
                            Ok(presigned) => {
                                if let Err(err) = upload_to_presigned(
                                    &presigned.url,
                                    "application/octet-stream",
                                    bytes,
                                )
                                .await
                                {
                                    eprintln!("supervise {run_id}: log upload failed: {err}");
                                }
                            }
                            Err(err) => eprintln!("supervise {run_id}: log presign failed: {err}"),
                        }
                    }
                }
                if let Err(err) = mirror_status(creds, &run_id, &status, &job.message).await {
                    eprintln!("supervise {run_id}: final status mirror failed: {err}");
                }
            }
            eprintln!("supervise {run_id}: finished ({status})");
            return Ok(());
        }

        if status != last_status {
            store.update_status(&run_id, &status, None, None)?;
            let cancel_requested = match &creds {
                Some(creds) => mirror_status(creds, &run_id, &status, &job.message)
                    .await
                    .unwrap_or(false),
                None => local_cancel_requested(&store, &run_id),
            };
            eprintln!("supervise {run_id}: {last_status} -> {status} (stage {stage})");
            last_status = status.clone();
            if cancel_requested && !cancel_sent {
                cancel_modal(&sandbox_id, &run_id, &mut cancel_sent).await;
            }
        } else if !cancel_sent {
            let cancel_requested = match &creds {
                Some(creds) => crate::client::get_external_run_state(creds, &run_id)
                    .await
                    .map(|s| s.cancel_requested)
                    .unwrap_or(false),
                None => local_cancel_requested(&store, &run_id),
            };
            if cancel_requested {
                cancel_modal(&sandbox_id, &run_id, &mut cancel_sent).await;
            }
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Modal twin of `tail_logs` — the launcher replays the sandbox's stdout from
/// the start on each connect, so the same truncate-and-dedup contract applies.
async fn tail_logs_modal(
    sandbox_id: String,
    path: std::path::PathBuf,
    run_id: String,
    done: tokio::sync::watch::Receiver<bool>,
) {
    let mut log_file = match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(err) => {
            eprintln!(
                "supervise {run_id}: could not open {}: {err}",
                path.display()
            );
            return;
        }
    };
    let mut seen = 0u64;
    loop {
        let mut sink = |line: &str| {
            let _ = writeln!(log_file, "{line}");
        };
        match modal::stream_logs(&sandbox_id, seen, LOG_IDLE, &mut sink).await {
            Ok(s) => seen = s,
            Err(err) => eprintln!("supervise {run_id}: log stream error (will retry): {err}"),
        }
        let _ = log_file.flush();
        if *done.borrow() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn cancel_modal(sandbox_id: &str, run_id: &str, cancel_sent: &mut bool) {
    eprintln!("supervise {run_id}: cancel requested — terminating modal sandbox");
    match modal::cancel_job(sandbox_id).await {
        Ok(()) => *cancel_sent = true,
        Err(err) => eprintln!("supervise {run_id}: modal cancel failed (will retry): {err}"),
    }
}

// --- ssh ----------------------------------------------------------------------
//
// Same two-half shape as the other backends, with `ssh` as the transport. The
// remote process has no scheduler; cancel TERMs its process group, which leaves
// it dead without an exit_code (ERROR) — so once cancel is sent we report the
// terminal state as `cancelled`.

async fn run_ssh(
    store: Store,
    stored: crate::store::StoredRun,
    descriptor: BackendDescriptor,
    creds: Option<Credentials>,
    run_id: String,
) -> Result<()> {
    let (host, dir) = descriptor.ssh_ref()?;
    eprintln!("supervise {run_id}: watching ssh job {host}:{dir}");
    let target = ssh::SshTarget::alias(host);
    let dir = dir.to_string();
    watch_ssh_job(&store, &stored.status, target, dir, &creds, &run_id).await?;
    Ok(())
}

/// The ssh two-half loop, shared by every backend whose job is a run dir on a
/// box we ssh into (ssh itself, openresearch). Runs until the job is terminal;
/// returns the final run status after logs are drained and mirrored.
async fn watch_ssh_job(
    store: &Store,
    initial_status: &str,
    target: ssh::SshTarget,
    dir: String,
    creds: &Option<Credentials>,
    run_id: &str,
) -> Result<String> {
    let path = log_path(run_id);
    let (done_tx, done_rx) = tokio::sync::watch::channel(false);
    let mut log_task = tokio::spawn(tail_logs_ssh(
        target.clone(),
        dir.clone(),
        path.clone(),
        run_id.to_string(),
        done_rx,
    ));

    let mut last_status = initial_status.to_string();
    let mut cancel_sent = false;

    loop {
        let job = match ssh::inspect_job(&target, &dir).await {
            Ok(j) => j,
            Err(err) => {
                eprintln!("supervise {run_id}: inspect failed (will retry): {err}");
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
        };
        let stage = job.stage.as_str();
        let status = if cancel_sent && is_terminal_stage(stage) {
            "cancelled".to_string()
        } else {
            stage_to_run_status(stage).to_string()
        };

        if is_terminal_stage(stage) {
            store.update_status(run_id, &status, Some(now_ms()), None)?;
            if creds.is_none() && status == "failed" {
                if let Some(msg) = &job.message {
                    if let Err(err) =
                        store.set_result_markdown(run_id, &format!("Job failed: {msg}"))
                    {
                        eprintln!("supervise {run_id}: could not record failure reason: {err}");
                    }
                }
            }
            let _ = done_tx.send(true);
            if tokio::time::timeout(Duration::from_secs(20), &mut log_task)
                .await
                .is_err()
            {
                log_task.abort();
            }
            if let Some(creds) = creds {
                if let Ok(bytes) = std::fs::read(&path) {
                    if !bytes.is_empty() {
                        match presign_external_run_log(creds, run_id).await {
                            Ok(presigned) => {
                                if let Err(err) = upload_to_presigned(
                                    &presigned.url,
                                    "application/octet-stream",
                                    bytes,
                                )
                                .await
                                {
                                    eprintln!("supervise {run_id}: log upload failed: {err}");
                                }
                            }
                            Err(err) => eprintln!("supervise {run_id}: log presign failed: {err}"),
                        }
                    }
                }
                if let Err(err) = mirror_status(creds, run_id, &status, &job.message).await {
                    eprintln!("supervise {run_id}: final status mirror failed: {err}");
                }
            }
            eprintln!("supervise {run_id}: finished ({status})");
            return Ok(status);
        }

        if status != last_status {
            store.update_status(run_id, &status, None, None)?;
            let cancel_requested = match creds {
                Some(creds) => mirror_status(creds, run_id, &status, &job.message)
                    .await
                    .unwrap_or(false),
                None => local_cancel_requested(store, run_id),
            };
            eprintln!("supervise {run_id}: {last_status} -> {status} (stage {stage})");
            last_status = status.clone();
            if cancel_requested && !cancel_sent {
                cancel_ssh(&target, &dir, run_id, &mut cancel_sent).await;
            }
        } else if !cancel_sent {
            let cancel_requested = match creds {
                Some(creds) => crate::client::get_external_run_state(creds, run_id)
                    .await
                    .map(|s| s.cancel_requested)
                    .unwrap_or(false),
                None => local_cancel_requested(store, run_id),
            };
            if cancel_requested {
                cancel_ssh(&target, &dir, run_id, &mut cancel_sent).await;
            }
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// SSH twin of `tail_logs` — each pass reads the remote log past the lines
/// already consumed, so the same truncate-and-dedup contract applies.
async fn tail_logs_ssh(
    target: ssh::SshTarget,
    dir: String,
    path: std::path::PathBuf,
    run_id: String,
    done: tokio::sync::watch::Receiver<bool>,
) {
    let mut log_file = match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(err) => {
            eprintln!(
                "supervise {run_id}: could not open {}: {err}",
                path.display()
            );
            return;
        }
    };
    let mut seen = 0u64;
    loop {
        let mut sink = |line: &str| {
            let _ = writeln!(log_file, "{line}");
        };
        match ssh::stream_logs(&target, &dir, seen, LOG_IDLE, &mut sink).await {
            Ok(s) => seen = s,
            Err(err) => eprintln!("supervise {run_id}: log stream error (will retry): {err}"),
        }
        let _ = log_file.flush();
        if *done.borrow() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn cancel_ssh(target: &ssh::SshTarget, dir: &str, run_id: &str, cancel_sent: &mut bool) {
    eprintln!("supervise {run_id}: cancel requested — killing remote process group");
    match ssh::cancel_job(target, dir).await {
        Ok(()) => *cancel_sent = true,
        Err(err) => eprintln!("supervise {run_id}: ssh cancel failed (will retry): {err}"),
    }
}

// --- openresearch ---------------------------------------------------------------
//
// The ssh loop with a provisioning prologue and a billing epilogue: the box
// comes from the platform, so the supervisor first waits for it to come online
// (recording the SSH endpoint on the descriptor for restarts), launches the
// payload over ssh, runs the shared watch loop, and deletes the box at the
// end. EVERY exit path tears the box down — a leaked box bills the org.

async fn run_openresearch(
    store: Store,
    stored: crate::store::StoredRun,
    mut descriptor: BackendDescriptor,
    creds: Option<Credentials>,
    run_id: String,
) -> Result<()> {
    let (_org, sandbox_id) = descriptor.openresearch_ref()?;
    let sandbox_id = sandbox_id.to_string();

    // Lifecycle credentials (poll/teardown) are the user's `orx login` token —
    // needed even though local runs skip the mirror (`creds`). Never
    // `require_credentials()` here: it exit(1)s, and dying silently in a
    // detached process would strand the run as "starting" and leak the box.
    let lifecycle = match crate::config::load_credentials().await {
        Ok(Some(c)) => c,
        _ => {
            store.update_status(&run_id, "failed", Some(now_ms()), None)?;
            store.set_result_markdown(
                &run_id,
                &format!(
                    "The supervisor found no OpenResearch credentials (`orx login`), so it \
                     could not manage box {sandbox_id} — the box may still be running; \
                     delete it from the dashboard."
                ),
            )?;
            return Err(anyhow!("no credentials for the openresearch backend"));
        }
    };

    let dir = openresearch::run_dir(&run_id);

    // Provisioning: wait for the box unless a restarted supervisor already
    // recorded its endpoint.
    let target = match descriptor.openresearch_ssh_target() {
        Some(target) => target,
        None => {
            eprintln!("supervise {run_id}: waiting for box {sandbox_id} to come online");
            let outcome = openresearch::wait_online(
                &lifecycle,
                &sandbox_id,
                openresearch::PROVISION_DEADLINE,
                || local_cancel_requested(&store, &run_id),
            )
            .await;
            let sandbox = match outcome {
                Ok(openresearch::WaitOutcome::Online(sandbox)) => sandbox,
                Ok(openresearch::WaitOutcome::Cancelled) => {
                    eprintln!("supervise {run_id}: cancelled during provisioning");
                    store.update_status(&run_id, "cancelled", Some(now_ms()), None)?;
                    teardown_box(&store, &lifecycle, &sandbox_id, &run_id).await;
                    return Ok(());
                }
                Err(err) => {
                    store.update_status(&run_id, "failed", Some(now_ms()), None)?;
                    store.set_result_markdown(&run_id, &format!("Provisioning failed: {err}"))?;
                    teardown_box(&store, &lifecycle, &sandbox_id, &run_id).await;
                    return Ok(());
                }
            };
            descriptor.ssh_host = sandbox.ssh_hostname.clone();
            descriptor.ssh_port = sandbox.ssh_port;
            descriptor.ssh_user = sandbox.ssh_username.clone();
            store.set_backend_json(&run_id, &descriptor.to_json())?;
            descriptor
                .openresearch_ssh_target()
                .ok_or_else(|| anyhow!("box {sandbox_id} came online without an SSH endpoint"))?
        }
    };

    // Launch, unless a previous supervisor already did (restart mid-run just
    // reattaches to the watch loop). An unreachable box reads as fresh here;
    // the launch retries below absorb that.
    let already_launched = openresearch::launched(&target, &run_id)
        .await
        .unwrap_or(false);
    if !already_launched {
        // The payload is re-derivable from the store + config, so a restart
        // that died before launching can rebuild it exactly.
        let Some(exp) = store.get_local_experiment(&stored.experiment_id)? else {
            store.update_status(&run_id, "failed", Some(now_ms()), None)?;
            store.set_result_markdown(
                &run_id,
                "Local experiment vanished from the store before launch.",
            )?;
            teardown_box(&store, &lifecycle, &sandbox_id, &run_id).await;
            return Ok(());
        };
        let Some(project) = store.get_local_project(&exp.project_id)? else {
            store.update_status(&run_id, "failed", Some(now_ms()), None)?;
            store.set_result_markdown(
                &run_id,
                "Local project vanished from the store before launch.",
            )?;
            teardown_box(&store, &lifecycle, &sandbox_id, &run_id).await;
            return Ok(());
        };
        let script = crate::commands::exp::hf_clone_script(
            &exp.branch_name,
            &project.github_owner,
            &project.github_repo,
            &stored.command,
        );
        let script =
            openresearch::wrap_with_timeout(&script, descriptor.timeout_secs.unwrap_or(4 * 3600));
        let mut env: std::collections::HashMap<String, String> =
            crate::config::list_synced_env().into_iter().collect();
        if let Ok(hf_token) = hf::resolve_token() {
            env.entry("HF_TOKEN".to_string()).or_insert(hf_token);
        }
        if let Some(gh) = crate::local::git::resolve_github_token() {
            env.insert("GITHUB_TOKEN".to_string(), gh);
        }

        // sshd and the org key sync can lag a freshly-online box, so the
        // launch retries for ~2 minutes before giving up.
        let mut launch_err = None;
        for backoff_secs in [0u64, 5, 10, 20, 30, 45] {
            if backoff_secs > 0 {
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
            }
            if local_cancel_requested(&store, &run_id) {
                eprintln!("supervise {run_id}: cancelled before launch");
                store.update_status(&run_id, "cancelled", Some(now_ms()), None)?;
                teardown_box(&store, &lifecycle, &sandbox_id, &run_id).await;
                return Ok(());
            }
            match ssh::run_job(&ssh::SshJobSpec {
                target: target.clone(),
                run_id: run_id.clone(),
                script: script.clone(),
                env: env.clone(),
            })
            .await
            {
                Ok(_) => {
                    launch_err = None;
                    break;
                }
                Err(err) => {
                    eprintln!("supervise {run_id}: launch failed (will retry): {err}");
                    launch_err = Some(err);
                }
            }
        }
        if let Some(err) = launch_err {
            store.update_status(&run_id, "failed", Some(now_ms()), None)?;
            store.set_result_markdown(
                &run_id,
                &format!("Could not launch the run on box {sandbox_id}: {err}"),
            )?;
            teardown_box(&store, &lifecycle, &sandbox_id, &run_id).await;
            return Ok(());
        }
    }

    eprintln!(
        "supervise {run_id}: watching openresearch box {sandbox_id} ({})",
        target.dest
    );
    // The shared ssh loop owns status/logs/mirror; the box is deleted after
    // it returns (logs are drained from the box BEFORE teardown), and even
    // when it errors.
    let watch = watch_ssh_job(&store, &stored.status, target, dir, &creds, &run_id).await;
    teardown_box(&store, &lifecycle, &sandbox_id, &run_id).await;
    watch?;
    Ok(())
}

/// Delete the run's box; on failure warn loudly and leave a cleanup hint on
/// the run. Teardown failure never changes the run's status — the run's
/// outcome and the box's fate are separate facts.
async fn teardown_box(store: &Store, creds: &Credentials, sandbox_id: &str, run_id: &str) {
    match openresearch::teardown(creds, sandbox_id).await {
        Ok(()) => eprintln!("supervise {run_id}: box {sandbox_id} deleted"),
        Err(err) => {
            eprintln!("supervise {run_id}: box {sandbox_id} could NOT be torn down: {err}");
            let existing = store
                .get_run(run_id)
                .ok()
                .flatten()
                .and_then(|r| r.result_markdown)
                .unwrap_or_default();
            let hint = format!(
                "\n\n> **Warning**: box {sandbox_id} could not be torn down ({err}) — it is \
                 still billing. Delete it with `orx instance delete {sandbox_id}` or from the \
                 dashboard."
            );
            let _ = store.set_result_markdown(run_id, &format!("{existing}{hint}"));
        }
    }
}

// --- local ---------------------------------------------------------------------
//
// The ssh loop with the transport removed: the run dir is on this machine, so
// inspect/log reads are plain fs calls. Same cancel semantics — TERM leaves the
// process dead without an exit_code (ERROR), reported as `cancelled`.

async fn run_local(
    store: Store,
    stored: crate::store::StoredRun,
    descriptor: BackendDescriptor,
    creds: Option<Credentials>,
    run_id: String,
) -> Result<()> {
    let dir = std::path::PathBuf::from(descriptor.local_ref()?);

    eprintln!("supervise {run_id}: watching local run {}", dir.display());

    let path = log_path(&run_id);
    let (done_tx, done_rx) = tokio::sync::watch::channel(false);
    let mut log_task = tokio::spawn(tail_logs_local(
        dir.clone(),
        path.clone(),
        run_id.clone(),
        done_rx,
    ));

    let mut last_status = stored.status.clone();
    let mut cancel_sent = false;

    loop {
        let job = localbox::inspect_job(&dir);
        let stage = job.stage.as_str();
        let status = if cancel_sent && is_terminal_stage(stage) {
            "cancelled".to_string()
        } else {
            stage_to_run_status(stage).to_string()
        };

        if is_terminal_stage(stage) {
            store.update_status(&run_id, &status, Some(now_ms()), None)?;
            if creds.is_none() && status == "failed" {
                if let Some(msg) = &job.message {
                    if let Err(err) =
                        store.set_result_markdown(&run_id, &format!("Job failed: {msg}"))
                    {
                        eprintln!("supervise {run_id}: could not record failure reason: {err}");
                    }
                }
            }
            let _ = done_tx.send(true);
            if tokio::time::timeout(Duration::from_secs(20), &mut log_task)
                .await
                .is_err()
            {
                log_task.abort();
            }
            if let Some(creds) = &creds {
                if let Err(err) = mirror_status(creds, &run_id, &status, &job.message).await {
                    eprintln!("supervise {run_id}: final status mirror failed: {err}");
                }
            }
            eprintln!("supervise {run_id}: finished ({status})");
            return Ok(());
        }

        if status != last_status {
            store.update_status(&run_id, &status, None, None)?;
            let cancel_requested = match &creds {
                Some(creds) => mirror_status(creds, &run_id, &status, &job.message)
                    .await
                    .unwrap_or(false),
                None => local_cancel_requested(&store, &run_id),
            };
            eprintln!("supervise {run_id}: {last_status} -> {status} (stage {stage})");
            last_status = status.clone();
            if cancel_requested && !cancel_sent {
                cancel_local(&dir, &run_id, &mut cancel_sent);
            }
        } else if !cancel_sent && local_cancel_requested(&store, &run_id) {
            cancel_local(&dir, &run_id, &mut cancel_sent);
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Local twin of `tail_logs_ssh` — mirrors the run dir's log into the store's
/// log file so `orx logs` and the dashboard read the usual place.
async fn tail_logs_local(
    dir: std::path::PathBuf,
    path: std::path::PathBuf,
    run_id: String,
    done: tokio::sync::watch::Receiver<bool>,
) {
    let mut log_file = match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(err) => {
            eprintln!(
                "supervise {run_id}: could not open {}: {err}",
                path.display()
            );
            return;
        }
    };
    let mut seen = 0u64;
    loop {
        let mut sink = |line: &str| {
            let _ = writeln!(log_file, "{line}");
        };
        match localbox::stream_logs(&dir, seen, &mut sink) {
            Ok(s) => seen = s,
            Err(err) => eprintln!("supervise {run_id}: log stream error (will retry): {err}"),
        }
        let _ = log_file.flush();
        if *done.borrow() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn cancel_local(dir: &std::path::Path, run_id: &str, cancel_sent: &mut bool) {
    eprintln!("supervise {run_id}: cancel requested — killing local process group");
    match localbox::cancel_job(dir) {
        Ok(()) => *cancel_sent = true,
        Err(err) => eprintln!("supervise {run_id}: local cancel failed (will retry): {err}"),
    }
}

// --- slurm ----------------------------------------------------------------------
//
// The ssh loop with a scheduler: state comes from the run dir's exit_code file
// first, then squeue/sacct; cancel is `scancel`. Logs reuse `tail_logs_ssh` —
// Slurm appends the job's output to the same `<run dir>/log` file the ssh
// backend uses. A scancel'd job leaves the queue without an exit_code, which
// inspect reports as CANCELED (or ERROR via the GONE fallback) — either way,
// once cancel is sent the terminal state maps to `cancelled`.

async fn run_slurm(
    store: Store,
    stored: crate::store::StoredRun,
    descriptor: BackendDescriptor,
    creds: Option<Credentials>,
    run_id: String,
) -> Result<()> {
    let (host, job_id) = descriptor.slurm_ref()?;
    let host = host.to_string();
    let job_id = job_id.to_string();
    let dir = slurm::run_dir(&run_id);

    eprintln!("supervise {run_id}: watching slurm job {job_id} on {host}");

    let path = log_path(&run_id);
    let (done_tx, done_rx) = tokio::sync::watch::channel(false);
    let mut log_task = tokio::spawn(tail_logs_ssh(
        ssh::SshTarget::alias(&host),
        dir.clone(),
        path.clone(),
        run_id.clone(),
        done_rx,
    ));

    let mut last_status = stored.status.clone();
    let mut cancel_sent = false;
    // "GONE" (scheduler doesn't know the job, no exit_code) must persist for
    // a full minute before it's believed: it also fires during slurmctld
    // restarts and while the exit_code write is NFS-lagged behind the compute
    // node. Any other observation resets the count.
    const GONE_POLLS_TO_FAIL: u32 = (60 / POLL_INTERVAL.as_secs()) as u32;
    let mut gone_polls = 0u32;

    loop {
        let mut job = match slurm::inspect_job(&host, &run_id, &job_id).await {
            Ok(j) => j,
            Err(err) => {
                eprintln!("supervise {run_id}: inspect failed (will retry): {err}");
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
        };
        if job.stage == "GONE" {
            gone_polls += 1;
            if gone_polls < GONE_POLLS_TO_FAIL {
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
            job = slurm::JobState {
                stage: "ERROR".to_string(),
                message: Some(
                    "job left the queue without an exit code (killed or node lost?)".to_string(),
                ),
            };
        } else {
            gone_polls = 0;
        }
        let stage = job.stage.as_str();
        let status = if cancel_sent && is_terminal_stage(stage) {
            "cancelled".to_string()
        } else {
            stage_to_run_status(stage).to_string()
        };

        if is_terminal_stage(stage) {
            store.update_status(&run_id, &status, Some(now_ms()), None)?;
            if creds.is_none() && status == "failed" {
                if let Some(msg) = &job.message {
                    if let Err(err) =
                        store.set_result_markdown(&run_id, &format!("Job failed: {msg}"))
                    {
                        eprintln!("supervise {run_id}: could not record failure reason: {err}");
                    }
                }
            }
            let _ = done_tx.send(true);
            if tokio::time::timeout(Duration::from_secs(20), &mut log_task)
                .await
                .is_err()
            {
                log_task.abort();
            }
            if let Some(creds) = &creds {
                if let Ok(bytes) = std::fs::read(&path) {
                    if !bytes.is_empty() {
                        match presign_external_run_log(creds, &run_id).await {
                            Ok(presigned) => {
                                if let Err(err) = upload_to_presigned(
                                    &presigned.url,
                                    "application/octet-stream",
                                    bytes,
                                )
                                .await
                                {
                                    eprintln!("supervise {run_id}: log upload failed: {err}");
                                }
                            }
                            Err(err) => eprintln!("supervise {run_id}: log presign failed: {err}"),
                        }
                    }
                }
                if let Err(err) = mirror_status(creds, &run_id, &status, &job.message).await {
                    eprintln!("supervise {run_id}: final status mirror failed: {err}");
                }
            }
            eprintln!("supervise {run_id}: finished ({status})");
            return Ok(());
        }

        if status != last_status {
            store.update_status(&run_id, &status, None, None)?;
            let cancel_requested = match &creds {
                Some(creds) => mirror_status(creds, &run_id, &status, &job.message)
                    .await
                    .unwrap_or(false),
                None => local_cancel_requested(&store, &run_id),
            };
            eprintln!("supervise {run_id}: {last_status} -> {status} (stage {stage})");
            last_status = status.clone();
            if cancel_requested && !cancel_sent {
                cancel_slurm(&host, &job_id, &run_id, &mut cancel_sent).await;
            }
        } else if !cancel_sent {
            let cancel_requested = match &creds {
                Some(creds) => crate::client::get_external_run_state(creds, &run_id)
                    .await
                    .map(|s| s.cancel_requested)
                    .unwrap_or(false),
                None => local_cancel_requested(&store, &run_id),
            };
            if cancel_requested {
                cancel_slurm(&host, &job_id, &run_id, &mut cancel_sent).await;
            }
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn cancel_slurm(host: &str, job_id: &str, run_id: &str, cancel_sent: &mut bool) {
    eprintln!("supervise {run_id}: cancel requested — scancel {job_id}");
    match slurm::cancel_job(host, job_id).await {
        Ok(()) => *cancel_sent = true,
        Err(err) => eprintln!("supervise {run_id}: scancel failed (will retry): {err}"),
    }
}
