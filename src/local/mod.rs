//! Local mode (`orx up`) — projects/experiments live in the local SQLite
//! store, experiment branches on the user's own GitHub repo, runs on HF Jobs.
//! Nothing under this module ever calls `client.rs` / the OpenResearch api —
//! except `openresearch`, whose *compute* is a platform box by definition
//! (the run rows still live only in the local store).
//!
//! Detection rule: an experiment/run is "local" iff its experiment id exists
//! in `local_experiments`. CLI commands check the local store FIRST and only
//! require credentials on the server path — dispatch on it via
//! `resolve::{resolve_project, resolve_experiment, resolve_run}`, never by hand.

pub mod agent_skills;
pub mod chat;
pub mod codex;
pub mod datadir;
pub mod experiments;
pub mod files;
pub mod git;
pub mod github;
pub mod harness;
pub mod hf;
pub mod k8s;
pub mod localrun;
pub mod modal;
pub mod model;
pub mod opencode;
pub mod openresearch;
pub mod projects;
pub mod resolve;
pub mod skills;
pub mod slurm;
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

/// Every backend a local-mode launch can target. The canonical id list —
/// launch dispatch, the default-target validation, and the Settings UI all
/// agree on these strings.
pub const BACKENDS: &[&str] = &[
    "local",
    "hf",
    "modal",
    "k8s",
    "ssh",
    "slurm",
    "openresearch",
];

/// Backends whose launches take a `--flavor` (hf/modal/openresearch require
/// one; slurm's is an optional GRES spec). k8s (manifest), ssh (host), and
/// local (this machine's hardware) have no flavor axis.
pub const FLAVORED_BACKENDS: &[&str] = &["hf", "modal", "slurm", "openresearch"];

/// The subset whose launches FAIL without a `--flavor` — the playbook warns
/// about these when they're the default with no saved flavor.
pub const FLAVOR_REQUIRED_BACKENDS: &[&str] = &["hf", "modal", "openresearch"];

/// Fill a local-mode launch's backend/flavor from the persisted default
/// (Settings → Compute) when the caller didn't pass them. Explicit args always
/// win; see `resolve_compute_default` for the exact precedence.
pub fn apply_compute_default(backend: &mut Option<String>, flavor: &mut Option<String>) {
    let (b, f) = resolve_compute_default(
        backend.take(),
        flavor.take(),
        crate::config::compute_default(),
    );
    *backend = b;
    *flavor = f;
}

/// Pure precedence rule for the default compute target:
/// - an explicit backend always wins; the default backend fills only when none
///   was given
/// - the default flavor fills only when the *effective* backend equals the
///   default backend AND no explicit flavor was given (a default flavor for
///   modal must never leak onto an explicit `--backend hf` launch)
/// - an explicit flavor is never overwritten
fn resolve_compute_default(
    explicit_backend: Option<String>,
    explicit_flavor: Option<String>,
    default: Option<(String, Option<String>)>,
) -> (Option<String>, Option<String>) {
    let Some((default_backend, default_flavor)) = default else {
        return (explicit_backend, explicit_flavor);
    };
    let backend = explicit_backend.unwrap_or_else(|| default_backend.clone());
    let flavor = explicit_flavor.or_else(|| {
        if backend == default_backend {
            default_flavor
        } else {
            None
        }
    });
    (Some(backend), flavor)
}

/// Validate a default-target choice before persisting it (the POST handler's
/// guard). Being *configured* is deliberately not required — config state
/// fluctuates outside orx and setup order shouldn't matter — but the backend
/// must exist and the flavor must be meaningful for it.
pub fn validate_compute_default(backend: &str, flavor: Option<&str>) -> Result<()> {
    if !BACKENDS.contains(&backend) {
        return Err(anyhow!(
            "Unknown backend '{backend}'. Valid backends: {}.",
            BACKENDS.join(", ")
        ));
    }
    if flavor.is_some_and(|f| !f.is_empty()) && !FLAVORED_BACKENDS.contains(&backend) {
        return Err(anyhow!(
            "Backend '{backend}' does not take a flavor (flavors apply to {}).",
            FLAVORED_BACKENDS.join(", ")
        ));
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn dflt(b: &str, f: Option<&str>) -> Option<(String, Option<String>)> {
        Some((b.to_string(), f.map(str::to_string)))
    }

    #[test]
    fn resolve_no_default_passes_through() {
        assert_eq!(
            resolve_compute_default(None, None, None),
            (None, None),
            "no default + no args stays empty (caller keeps its error path)"
        );
        assert_eq!(
            resolve_compute_default(Some("hf".into()), Some("t4-small".into()), None),
            (Some("hf".into()), Some("t4-small".into()))
        );
    }

    #[test]
    fn resolve_default_fills_omitted_backend_and_flavor() {
        assert_eq!(
            resolve_compute_default(None, None, dflt("modal", Some("a10g"))),
            (Some("modal".into()), Some("a10g".into()))
        );
        assert_eq!(
            resolve_compute_default(None, None, dflt("local", None)),
            (Some("local".into()), None)
        );
    }

    #[test]
    fn resolve_explicit_backend_wins_and_blocks_default_flavor() {
        // Explicit backend differs from the default: the default flavor must
        // NOT leak onto it.
        assert_eq!(
            resolve_compute_default(Some("hf".into()), None, dflt("modal", Some("a10g"))),
            (Some("hf".into()), None)
        );
        // Explicit backend HAPPENS to equal the default: flavor still fills.
        assert_eq!(
            resolve_compute_default(Some("modal".into()), None, dflt("modal", Some("a10g"))),
            (Some("modal".into()), Some("a10g".into()))
        );
    }

    #[test]
    fn resolve_explicit_flavor_never_overwritten() {
        assert_eq!(
            resolve_compute_default(None, Some("h100".into()), dflt("modal", Some("a10g"))),
            (Some("modal".into()), Some("h100".into()))
        );
    }

    #[test]
    fn validate_default_backend_ids_and_flavors() {
        for b in BACKENDS {
            assert!(validate_compute_default(b, None).is_ok(), "{b} valid");
        }
        assert!(validate_compute_default("gcp", None).is_err());
        for b in FLAVORED_BACKENDS {
            assert!(
                validate_compute_default(b, Some("x")).is_ok(),
                "{b} flavored"
            );
        }
        for b in ["k8s", "ssh", "local"] {
            assert!(
                validate_compute_default(b, Some("x")).is_err(),
                "{b} must reject a flavor"
            );
            assert!(
                validate_compute_default(b, Some("")).is_ok(),
                "{b} tolerates an empty flavor (treated as absent)"
            );
        }
    }
}
