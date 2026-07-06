//! Local mode (`orx up`) — projects/experiments live in the local SQLite
//! store, experiment branches on the user's own GitHub repo, runs on HF Jobs.
//! Nothing under this module ever calls `client.rs` / the OpenResearch api.
//!
//! Detection rule: an experiment/run is "local" iff its experiment id exists
//! in `local_experiments`. CLI commands check the local store FIRST and only
//! require credentials on the server path.

pub mod artifacts;
pub mod chat;
pub mod experiments;
pub mod git;
pub mod github;
pub mod harness;
pub mod hf;
pub mod k8s;
pub mod modal;
pub mod model;
pub mod opencode;
pub mod projects;
pub mod ssh;

use crate::error::{anyhow, Error, Result};
use crate::store::{now_ms, Store, StoredRun};

/// Graceful error for commands outside the local-mode v1 surface.
pub fn unsupported(cmd: &str) -> Error {
    anyhow!(
        "`orx {cmd}` is not supported in local mode yet.\n\
         Local mode supports: projects, project view/edit, create-experiment, \
         exp run/status/cancel/wait/desc, runs, logs."
    )
}

/// Terminal run states — the run is finished and won't change further.
pub fn is_terminal(status: &str) -> bool {
    matches!(status, "done" | "failed" | "cancelled")
}

/// The stored run, but only when it belongs to a local experiment. Server-mode
/// HF runs also live in the runs table, so membership there is not enough.
pub fn local_run(store: &Store, run_id: &str) -> Result<Option<StoredRun>> {
    match store.get_run(run_id)? {
        Some(run) => Ok(store.get_local_experiment(&run.experiment_id)?.map(|_| run)),
        None => Ok(None),
    }
}

/// Lowercased, dash-separated slug from free text (branch- and URL-safe).
pub fn slugify(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out: String = out.trim_matches('-').chars().take(48).collect();
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "experiment".to_string()
    } else {
        out
    }
}

/// Compact relative time for local tables ("3m ago").
pub fn fmt_ago(ms: i64) -> String {
    let secs = (now_ms() - ms).max(0) / 1000;
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// Run duration in seconds (open-ended runs measure up to now).
pub fn run_duration_secs(run: &StoredRun) -> i64 {
    (run.ended_at.unwrap_or_else(now_ms) - run.created_at) / 1000
}

/// Local twin of `output::run_failure_detail` for store-backed runs.
pub fn run_failure_detail(run: &StoredRun) -> Option<String> {
    if run.status != "failed" {
        return None;
    }
    match run.result_markdown.as_deref().map(str::trim) {
        Some(reason) if !reason.is_empty() => Some(format!("reason: {reason}")),
        _ => Some(format!(
            "reason: — (no message recorded — see `orx logs {}`)",
            run.id
        )),
    }
}
