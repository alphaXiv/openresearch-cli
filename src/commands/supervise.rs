//! `orx supervise <runId>` — the lens beside an external job.
//!
//! Spawned detached by `orx exp run --backend hf`; restart-idempotent (state
//! is the local store + the backend itself, and log dedup resumes from the
//! store's log file). Loop: tail backend logs into the run's log file, poll
//! job state, mirror transitions to the api (whose PATCH response carries
//! cancel intent), and on terminal status upload the log via presigned PUT.
//!
//! API unreachability never kills supervision: the local store stays correct
//! and mirroring resumes on the next transition.

use std::io::Write as _;
use std::time::Duration;

use serde_json::json;

use crate::client::{presign_external_run_log, update_external_run, upload_to_presigned};
use crate::config::Credentials;
use crate::error::{anyhow, require_credentials, Result};
use crate::jobs::huggingface as hf;
use crate::jobs::{is_terminal_stage, stage_to_run_status, BackendDescriptor};
use crate::store::{log_path, now_ms, Store};

const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// How long a silent log stream is held before re-checking job state.
const LOG_IDLE: Duration = Duration::from_secs(30);

pub async fn run(args: crate::SuperviseArgs) -> Result<()> {
    let creds = require_credentials().await;
    let run_id = args.run_id;

    let store = Store::open()?;
    let stored = store
        .get_run(&run_id)?
        .ok_or_else(|| anyhow!("Run {} not found in the local store.", run_id))?;
    let descriptor = BackendDescriptor::parse(&stored.backend_json)?;
    let (namespace, job_id) = descriptor.hf_ref()?;
    let namespace = namespace.to_string();
    let job_id = job_id.to_string();
    let token = hf::resolve_token()?;

    eprintln!("supervise {run_id}: watching hf job {namespace}/{job_id}");

    // Resume-aware log sink: append to the run's log file. `events_seen` dedups
    // the SSE replay across reconnects; on a supervisor restart we conservatively
    // start from 0 events but truncate the file first so the file never doubles.
    let path = log_path(&run_id);
    let mut log_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .map_err(|e| anyhow!("Could not open {}: {}", path.display(), e))?;
    let mut events_seen = 0u64;

    let mut last_status = stored.status.clone();
    let mut cancel_sent = false;

    loop {
        // 1) Drain whatever logs are available (returns on idle/close).
        let mut sink = |line: &str| {
            let _ = writeln!(log_file, "{line}");
        };
        match hf::stream_logs(
            &token,
            &namespace,
            &job_id,
            events_seen,
            LOG_IDLE,
            &mut sink,
        )
        .await
        {
            Ok(seen) => events_seen = seen,
            Err(err) => eprintln!("supervise {run_id}: log stream error (will retry): {err}"),
        }
        let _ = log_file.flush();

        // 2) Where is the job now?
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

        // 3) Terminal: persist everything BEFORE reporting the final status, so
        // the moment the UI sees done/failed the R2 log already exists (the
        // run page switches from live tail to the persisted log on that flip).
        if is_terminal_stage(stage) {
            store.update_status(&run_id, &status, Some(now_ms()), None)?;
            let _ = log_file.flush();
            if let Ok(bytes) = std::fs::read(&path) {
                if !bytes.is_empty() {
                    match presign_external_run_log(&creds, &run_id).await {
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
            if let Err(err) = mirror_status(&creds, &run_id, &status, &job.status.message).await {
                eprintln!("supervise {run_id}: final status mirror failed: {err}");
            }
            eprintln!("supervise {run_id}: finished ({status})");
            return Ok(());
        }

        // 3b) Mirror a live transition (local store first — it's the truth).
        if status != last_status {
            store.update_status(&run_id, &status, None, None)?;
            let cancel_requested = mirror_status(&creds, &run_id, &status, &job.status.message)
                .await
                .unwrap_or(false);
            eprintln!("supervise {run_id}: {last_status} -> {status} (stage {stage})");
            last_status = status.clone();
            if cancel_requested && !cancel_sent {
                request_backend_cancel(&token, &namespace, &job_id, &run_id, &mut cancel_sent)
                    .await;
            }
        } else if !cancel_sent {
            // No transition to report — poll cancel intent cheaply instead.
            if let Ok(state) = crate::client::get_external_run_state(&creds, &run_id).await {
                if state.cancel_requested {
                    request_backend_cancel(&token, &namespace, &job_id, &run_id, &mut cancel_sent)
                        .await;
                }
            }
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
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
