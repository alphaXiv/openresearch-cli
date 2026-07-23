//! The control-plane abstraction — one `ControlPlane` trait with two
//! implementors, `ServerPlane` (the cloud api, `client.rs`) and `LocalPlane`
//! (the local store + `src/local`). The six dual-mode commands
//! (`exp`/`runs`/`logs`/`project`/`report`/`create_experiment`) resolve an id to
//! a `Box<dyn ControlPlane>` once and then call verbs, instead of each branching
//! on `resolve_project`/`resolve_run` and inlining a local body and a server body.
//!
//! ## Why this lives outside `src/local/`
//!
//! `local/mod.rs` documents a hard invariant: nothing under `src/local/` ever
//! calls `client.rs`. A `ServerPlane` is by definition a `client.rs` wrapper, so
//! it cannot live there. `local::resolve` stays the pure "is this id local?"
//! decision layer; `plane::resolve_*` builds on it and hands back the boxed plane
//! that owns the id.
//!
//! ## What the domain types deliberately exclude
//!
//! `plane::{Run, Experiment, Project}` are CLI-facing only: they carry exactly
//! the fields the six commands' render code reads, and nothing else. They are NOT
//! the `orx up` HTTP wire types — `local::model::*` (camelCase, consumed by the
//! UI) and the `client.rs` DTOs keep their own shapes and serialization. Each
//! plane maps its native type (`client::Run` / `store::StoredRun`, …) into the
//! domain type at its own boundary via plain conversions here. This confines the
//! type change to CLI table output, which is snapshot-verifiable, and keeps the
//! wire (UI) and SQLite schema untouched.

use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::client;
use crate::error::{anyhow, Result};
use crate::store::{Store, StoredRun};

// ---------------------------------------------------------------------------
// Domain types — CLI-facing projections of the parallel wire/row shapes.
// ---------------------------------------------------------------------------

/// Which plane a domain `Run` came from. Only affects the no-persisted-reason
/// wording in `failure_detail` (the two originals diverged there); everything
/// else about a `Run` is plane-agnostic.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RunOrigin {
    /// From `client::Run`. `has_log` distinguishes "failed after startup" (a
    /// captured log key exists) from a bare "no message recorded".
    Server { has_log: bool },
    /// From `store::StoredRun`. No server log-key concept; the no-reason wording
    /// always points at `orx logs`.
    Local,
}

/// A run, projected to just the fields CLI rendering reads. `duration_secs` and
/// `updated_display` are pre-rendered by the owning plane because the two planes
/// compute them differently (server: server-supplied seconds + an ISO string;
/// local: `now - created` and a relative "3m ago") — folding that into the
/// domain type keeps the command's table code plane-agnostic and byte-identical.
pub struct Run {
    pub id: String,
    pub experiment_id: String,
    pub status: String,
    pub commit_sha: Option<String>,
    /// Seconds from creation to end (or to now while in-flight).
    pub duration_secs: i64,
    /// Ready-to-print "updated" cell (ISO string on server, "3m ago" locally).
    pub updated_display: String,
    /// Terminal detail source for `failure_detail`: the persisted reason.
    pub result_markdown: Option<String>,
    /// Which plane produced this run — selects the no-reason failure wording.
    pub origin: RunOrigin,
}

impl From<client::Run> for Run {
    fn from(r: client::Run) -> Self {
        Run {
            origin: RunOrigin::Server {
                has_log: r.log_key.is_some(),
            },
            id: r.id,
            experiment_id: r.experiment_id,
            status: r.status,
            commit_sha: r.commit_sha,
            duration_secs: r.duration_seconds,
            updated_display: r.updated_at,
            result_markdown: r.result_markdown,
        }
    }
}

impl From<&StoredRun> for Run {
    fn from(r: &StoredRun) -> Self {
        Run {
            id: r.id.clone(),
            experiment_id: r.experiment_id.clone(),
            status: r.status.clone(),
            commit_sha: r.commit_sha.clone(),
            duration_secs: crate::local::run_duration_secs(r),
            updated_display: crate::local::fmt_ago(r.updated_at),
            result_markdown: r.result_markdown.clone(),
            origin: RunOrigin::Local,
        }
    }
}

impl Run {
    /// When a run ended in `failed`, a human-readable explanation to print
    /// beneath its status line — so an agent driving the CLI sees *why* a run
    /// died, not just that it did. For compute spin-up failures this is the same
    /// provider error the website shows as a toast (persisted to
    /// `result_markdown`). Otherwise no reason is recorded, so we point at the
    /// logs. Returns `None` for non-failed runs.
    ///
    /// This is the single merge of the former `output::run_failure_detail`
    /// (`client::Run`) and `local::run_failure_detail` (`StoredRun`). They agreed
    /// on the reason-present branch and diverged only when no reason was
    /// persisted; `origin` reproduces each wording exactly.
    pub fn failure_detail(&self) -> Option<String> {
        if self.status != "failed" {
            return None;
        }
        match self.result_markdown.as_deref().map(str::trim) {
            Some(reason) if !reason.is_empty() => Some(format!("reason: {reason}")),
            // No persisted reason: the server path split "failed after startup"
            // (a captured log exists) from a bare "no message recorded"; the
            // local path always points at `orx logs`.
            _ => Some(match self.origin {
                RunOrigin::Server { has_log: true } => format!(
                    "reason: — (failed after startup; no message recorded — see `orx logs {}`)",
                    self.id
                ),
                RunOrigin::Server { has_log: false } => {
                    "reason: — (no message recorded)".to_string()
                }
                RunOrigin::Local => format!(
                    "reason: — (no message recorded — see `orx logs {}`)",
                    self.id
                ),
            }),
        }
    }
}

/// A rendered run log excerpt (server byte-range read or local file read),
/// projected to the fields `orx logs` prints. The command owns the stdout write
/// and the stderr footer; the plane supplies the bytes and the window metadata.
pub struct RunLog {
    /// Log bytes for the requested window.
    pub content: Vec<u8>,
    pub start_byte: i64,
    pub end_byte: i64,
    pub total_bytes: i64,
    /// Footer source label: the server's `source`, or "local file".
    pub source: String,
    pub truncated_before: bool,
    pub truncated_after: bool,
    /// Set only on the local path when the file does not exist yet — the command
    /// prints the "no log captured yet" line instead of an empty read.
    pub missing_local: bool,
}

/// The two shapes of `orx logs` footer, kept byte-identical to the originals.
impl RunLog {
    /// `"[<source>] bytes <s>–<e> of <total>[ (more above, more below)]"`.
    pub fn footer(&self) -> String {
        let mut more: Vec<&str> = Vec::new();
        if self.truncated_before {
            more.push("more above");
        }
        if self.truncated_after {
            more.push("more below");
        }
        let more_str = if more.is_empty() {
            String::new()
        } else {
            format!(" ({})", more.join(", "))
        };
        format!(
            "[{}] bytes {}–{} of {}{}",
            self.source, self.start_byte, self.end_byte, self.total_bytes, more_str
        )
    }
}

/// The description write vs read distinction. Resolved *inside* each plane's
/// verb — not by the command — so ordering vs the login check matches the
/// pre-trait code: a logged-out `--stdin` user gets "Not logged in" without
/// stdin being consumed first (the server plane connects before resolving).
pub enum DescInput {
    /// Overwrite the description with this text.
    Set(String),
    /// Read/print the current description.
    Get,
}

impl DescInput {
    /// `--set` and `--stdin` are mutually exclusive; either present means
    /// "overwrite". `--stdin` reads to EOF here, which is why call order
    /// relative to `require_credentials` matters (see the enum doc).
    pub async fn resolve(set: Option<String>, stdin: bool) -> Result<Self> {
        use tokio::io::AsyncReadExt as _;
        match (set, stdin) {
            (Some(_), true) => Err(anyhow!("Pass either --set or --stdin, not both.")),
            (Some(text), false) => Ok(DescInput::Set(text)),
            (None, true) => {
                let mut buf = String::new();
                tokio::io::stdin().read_to_string(&mut buf).await?;
                Ok(DescInput::Set(buf))
            }
            (None, false) => Ok(DescInput::Get),
        }
    }
}

// ---------------------------------------------------------------------------
// The trait.
// ---------------------------------------------------------------------------

/// One control plane (cloud api or local store). The verb set is derived
/// mechanically from what the six dual-mode commands call across their two arms.
///
/// Each verb owns the behavior of one command arm, including its printing where
/// the two planes print differently (status lines, launch recaps, the logs
/// footer): merging that printing would risk drift, so it stays in the impl and
/// the command keeps only shared arg parsing / usage errors / shared table code.
/// Verbs that are server-only today (`set_experiment_command`, all `report_*`,
/// `create_child`) return the SAME `local::unsupported`/guidance error on
/// `LocalPlane` that the command returns today — byte-identical.
///
/// `?Send`: `LocalPlane` owns a `rusqlite`-backed `Store` (which is `!Sync`), so
/// its verb futures can't be `Send`. That's fine — a plane is built and driven to
/// completion inline within one command's `async fn` (the `#[tokio::main]` future
/// is `block_on`, which needs no `Send`); a plane is never `tokio::spawn`ed or
/// moved across threads. Unlike the `Harness` trait (a `Sync` static registry),
/// nothing shares a `ControlPlane` between tasks.
#[async_trait(?Send)]
pub trait ControlPlane {
    /// Whether this is the local store plane. Only for the create-experiment
    /// telemetry flag (`capture_experiment_started(_, is_local, _)`), which the
    /// command fires after the verb returns Ok.
    fn is_local(&self) -> bool;

    // --- project ----------------------------------------------------------

    /// `orx project view <id>` — print the project overview.
    async fn view_project(&self) -> Result<()>;

    /// `orx project edit <id>` — local accepts name/run_command; server accepts
    /// name/description/visibility. The command validates the per-plane flag
    /// combination before calling (each plane rejects the flags it can't take).
    async fn edit_project(&self, edit: ProjectEdit) -> Result<()>;

    // --- runs / logs ------------------------------------------------------

    /// `orx runs <id>` — the project's runs plus an experiment-id→title map for
    /// labeling. Newest first. The command renders the shared table.
    async fn list_runs(&self) -> Result<RunListing>;

    /// `orx logs <runId>` — a log window for the resolved run.
    async fn read_log(&self, req: LogRequest) -> Result<RunLog>;

    // --- experiment -------------------------------------------------------

    /// `orx exp status <expId>` — print the experiment's status block.
    async fn experiment_status(&self) -> Result<()>;

    /// `orx exp desc <expId>` — read or write the description. Takes the raw
    /// flags so each plane resolves stdin at the point the pre-trait code did
    /// (server: after the login check).
    async fn experiment_desc(&self, set: Option<String>, stdin: bool) -> Result<()>;

    /// `orx exp cmd <expId> --set` — set the run command (server only; local
    /// returns `unsupported("exp cmd")`).
    async fn set_experiment_command(&self, command: Option<String>) -> Result<()>;

    /// `orx exp run <expId> …` — launch a run. The command passes the parsed
    /// `ExpRunArgs`; each plane validates and dispatches its own backends.
    async fn launch(&self, args: crate::ExpRunArgs) -> Result<()>;

    /// `orx exp cancel <expId>` — cancel the in-flight run(s).
    async fn cancel(&self) -> Result<()>;

    /// `orx exp wait <expId>` — block on the experiment's latest run until
    /// terminal (level trigger).
    async fn wait_experiment(&self, interval: Duration, deadline: Instant) -> Result<()>;

    /// `orx exp wait --project <projectId>` — return on the first completion
    /// (edge trigger). Keyed on a project id, so it's a plane built from the
    /// project ref.
    async fn wait_project(&self, interval: Duration, deadline: Instant) -> Result<()>;

    // --- create-experiment ------------------------------------------------

    /// `orx create-experiment <projectId> …` — create a child or baseline node.
    /// The command owns the USAGE guard and fires telemetry after Ok.
    async fn create_experiment(&self, spec: CreateExperimentSpec) -> Result<()>;

    // --- reports (server only; local returns guidance) --------------------

    /// `orx report <projectId> …` — dispatch the report subcommand.
    async fn report(&self, cmd: crate::ReportCommand) -> Result<()>;
}

/// The resolved fields for `orx project edit`. Local rejects the server-only
/// fields and vice-versa (the command pre-validates the combination).
pub struct ProjectEdit {
    pub name: Option<String>,
    pub description: Option<String>,
    pub description_stdin: bool,
    pub public: bool,
    pub private: bool,
    pub run_command: Option<String>,
}

/// The runs listing for `orx runs`: the domain runs plus the experiment
/// id→display-title map used to label each row.
pub struct RunListing {
    pub runs: Vec<Run>,
    pub titles: std::collections::HashMap<String, String>,
}

/// A parsed `orx logs` request (mode + byte window), resolved from args by the
/// command's shared parsing.
pub struct LogRequest {
    pub mode: String,
    pub max_bytes: Option<i64>,
    pub start_byte: Option<i64>,
    pub end_byte: Option<i64>,
}

/// The resolved `orx create-experiment` inputs (title already required/parsed).
pub struct CreateExperimentSpec {
    pub title: String,
    pub parent: Option<String>,
    pub baseline: bool,
    pub description: Option<String>,
    pub run_command: Option<String>,
}

// ---------------------------------------------------------------------------
// Resolvers — build the boxed plane that owns an id.
// ---------------------------------------------------------------------------

/// Resolve a project-keyed command to its plane. Local iff the id names a known
/// local project (`local::resolve::resolve_project`).
pub fn resolve_project(store: Store, project_id: &str) -> Result<Box<dyn ControlPlane>> {
    use crate::local::resolve::{resolve_project, ProjectRef};
    Ok(match resolve_project(&store, project_id)? {
        ProjectRef::Local(project) => Box::new(LocalPlane {
            store,
            project: Some(*project),
            experiment: None,
            id: project_id.to_string(),
        }),
        ProjectRef::Server(id) => Box::new(ServerPlaceholder { id }),
    })
}

/// Resolve an experiment-keyed command to its plane. Local iff the id names a
/// known local experiment (`local::resolve::resolve_experiment`).
pub fn resolve_experiment(store: Store, exp_id: &str) -> Result<Box<dyn ControlPlane>> {
    use crate::local::resolve::{resolve_experiment, ExperimentRef};
    Ok(match resolve_experiment(&store, exp_id)? {
        ExperimentRef::Local(exp) => Box::new(LocalPlane {
            store,
            project: None,
            experiment: Some(*exp),
            id: exp_id.to_string(),
        }),
        ExperimentRef::Server(id) => Box::new(ServerPlaceholder { id }),
    })
}

/// Resolve a run-keyed command to its plane. Local iff the run belongs to a
/// local experiment (`local::resolve::resolve_run`, which reuses `local_run`).
pub fn resolve_run(store: Store, run_id: &str) -> Result<Box<dyn ControlPlane>> {
    use crate::local::resolve::{resolve_run, RunRef};
    Ok(match resolve_run(&store, run_id)? {
        RunRef::Local(_) => Box::new(LocalPlane {
            store,
            project: None,
            experiment: None,
            id: run_id.to_string(),
        }),
        RunRef::Server(id) => Box::new(ServerPlaceholder { id }),
    })
}

mod local_plane;
mod server_plane;

use local_plane::LocalPlane;
use server_plane::ServerPlaceholder;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{now_ms, StoredRun};

    fn stored_run(status: &str, result_markdown: Option<&str>) -> StoredRun {
        let now = now_ms();
        StoredRun {
            id: "r1".to_string(),
            experiment_id: "e1".to_string(),
            project_id: "p1".to_string(),
            status: status.to_string(),
            backend_json: "{}".to_string(),
            command: "echo hi".to_string(),
            created_at: now,
            updated_at: now,
            ended_at: Some(now),
            exit_code: None,
            commit_sha: Some("abcdef1234567890".to_string()),
            result_markdown: result_markdown.map(str::to_string),
            cancel_requested: false,
            chat_session_id: None,
        }
    }

    #[test]
    fn stored_run_maps_to_local_domain_run() {
        let run = Run::from(&stored_run("done", None));
        assert_eq!(run.id, "r1");
        assert_eq!(run.experiment_id, "e1");
        assert_eq!(run.status, "done");
        assert_eq!(run.commit_sha.as_deref(), Some("abcdef1234567890"));
        assert!(matches!(run.origin, RunOrigin::Local));
        // The store keeps unix millis; the local mapping renders a relative
        // "ago" string, not a raw timestamp.
        assert!(run.updated_display.ends_with("ago"));
    }

    #[test]
    fn failure_detail_none_for_non_failed() {
        assert!(Run::from(&stored_run("done", None))
            .failure_detail()
            .is_none());
    }

    #[test]
    fn failure_detail_reports_persisted_reason() {
        let run = Run::from(&stored_run("failed", Some("  boom  ")));
        assert_eq!(run.failure_detail().as_deref(), Some("reason: boom"));
    }

    #[test]
    fn failure_detail_local_no_reason_points_at_logs() {
        // A store run always has RunOrigin::Local: the no-reason wording must be
        // the former `local::run_failure_detail` output, byte-for-byte.
        let run = Run::from(&stored_run("failed", None));
        assert_eq!(
            run.failure_detail().as_deref(),
            Some("reason: — (no message recorded — see `orx logs r1`)")
        );
    }

    #[test]
    fn failure_detail_server_variants_match_the_old_output_fn() {
        // Reproduce the two former `output::run_failure_detail` no-reason
        // branches via RunOrigin::Server { has_log }.
        let with_log = Run {
            id: "r1".to_string(),
            experiment_id: "e1".to_string(),
            status: "failed".to_string(),
            commit_sha: None,
            duration_secs: 0,
            updated_display: String::new(),
            result_markdown: None,
            origin: RunOrigin::Server { has_log: true },
        };
        assert_eq!(
            with_log.failure_detail().as_deref(),
            Some("reason: — (failed after startup; no message recorded — see `orx logs r1`)")
        );
        let no_log = Run {
            origin: RunOrigin::Server { has_log: false },
            ..with_log
        };
        assert_eq!(
            no_log.failure_detail().as_deref(),
            Some("reason: — (no message recorded)")
        );
    }
}
