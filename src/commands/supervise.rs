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
use crate::jobs::modal;
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
                    &job_name,
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
                    &job_name,
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
/// each reconnect, so the same truncate-and-dedup contract applies.
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
    job_name: &str,
    run_id: &str,
    cancel_sent: &mut bool,
) {
    eprintln!("supervise {run_id}: cancel requested — deleting k8s job");
    match k8s::cancel_job(context, namespace, job_name).await {
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
    let host = host.to_string();
    let dir = dir.to_string();

    eprintln!("supervise {run_id}: watching ssh job {host}:{dir}");

    let path = log_path(&run_id);
    let (done_tx, done_rx) = tokio::sync::watch::channel(false);
    let mut log_task = tokio::spawn(tail_logs_ssh(
        host.clone(),
        dir.clone(),
        path.clone(),
        run_id.clone(),
        done_rx,
    ));

    let mut last_status = stored.status.clone();
    let mut cancel_sent = false;

    loop {
        let job = match ssh::inspect_job(&host, &dir).await {
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
                cancel_ssh(&host, &dir, &run_id, &mut cancel_sent).await;
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
                cancel_ssh(&host, &dir, &run_id, &mut cancel_sent).await;
            }
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// SSH twin of `tail_logs` — each pass reads the remote log past the lines
/// already consumed, so the same truncate-and-dedup contract applies.
async fn tail_logs_ssh(
    host: String,
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
        match ssh::stream_logs(&host, &dir, seen, LOG_IDLE, &mut sink).await {
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

async fn cancel_ssh(host: &str, dir: &str, run_id: &str, cancel_sent: &mut bool) {
    eprintln!("supervise {run_id}: cancel requested — killing remote process group");
    match ssh::cancel_job(host, dir).await {
        Ok(()) => *cancel_sent = true,
        Err(err) => eprintln!("supervise {run_id}: ssh cancel failed (will retry): {err}"),
    }
}
