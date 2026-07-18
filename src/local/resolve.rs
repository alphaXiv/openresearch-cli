//! Control-plane resolver — the single place that answers "does this id belong
//! to the local store or the server (cloud) api?".
//!
//! The detection rule is documented in `local/mod.rs`: an experiment/run is
//! "local" iff its experiment id exists in `local_experiments`. Every dual-mode
//! and reject-guard command dispatches through here so the rule lives in one
//! place. `resolve_project` keys on `local_projects`, `resolve_experiment` on
//! `local_experiments`. Note that store membership alone is NOT enough for runs
//! — server-mode HF runs also live in the `runs` table — so `resolve_run` reuses
//! the existing `local::local_run` correctness test rather than re-deriving it.

use crate::error::Result;
use crate::local::model::{LocalExperiment, LocalProject};
use crate::store::{Store, StoredRun};

/// Which control plane owns a given project id.
pub enum ProjectRef {
    /// The id resolves to a row in `local_projects`; the fetched project is
    /// carried through so the local arm needs no second store lookup. Boxed so
    /// the enum isn't dominated by `LocalProject`'s size (the common arm is
    /// `Server`).
    Local(Box<LocalProject>),
    /// The id is not local; treat it as a server (api) project.
    Server(String),
}

/// Which control plane owns a given experiment id.
pub enum ExperimentRef {
    /// The id resolves to a row in `local_experiments`; the fetched experiment
    /// is carried through so the local arm needs no second store lookup. Boxed
    /// so the enum isn't dominated by `LocalExperiment`'s size (the common arm
    /// is `Server`).
    Local(Box<LocalExperiment>),
    /// The id is not local; treat it as a server (api) experiment.
    Server(String),
}

/// Which control plane owns a given run id.
pub enum RunRef {
    /// The run belongs to a local experiment (per `local::local_run`); the
    /// fetched run is carried through for call sites that need it. Boxed so the
    /// enum isn't dominated by `StoredRun`'s size (the common arm is `Server`).
    Local(Box<StoredRun>),
    /// The run is not local (or does not exist locally); treat it as a server run.
    Server(String),
}

impl ProjectRef {
    /// Guard form for reject-only call sites that don't consume the project.
    pub fn is_local(&self) -> bool {
        matches!(self, ProjectRef::Local(_))
    }
}

impl RunRef {
    /// Guard form for reject-only call sites that don't consume the run.
    pub fn is_local(&self) -> bool {
        matches!(self, RunRef::Local(_))
    }
}

/// Decide once: a project id is local iff it names a known local project.
pub fn resolve_project(store: &Store, project_id: &str) -> Result<ProjectRef> {
    match store.get_local_project(project_id)? {
        Some(p) => Ok(ProjectRef::Local(Box::new(p))),
        None => Ok(ProjectRef::Server(project_id.to_string())),
    }
}

/// Decide once: an experiment id is local iff it names a known local experiment.
pub fn resolve_experiment(store: &Store, exp_id: &str) -> Result<ExperimentRef> {
    match store.get_local_experiment(exp_id)? {
        Some(e) => Ok(ExperimentRef::Local(Box::new(e))),
        None => Ok(ExperimentRef::Server(exp_id.to_string())),
    }
}

/// Decide once for a run id. Reuses `local::local_run`, which encodes the
/// correct test (the run's experiment must be in `local_experiments`) — store
/// membership alone is not enough, since server HF runs also live in `runs`.
pub fn resolve_run(store: &Store, run_id: &str) -> Result<RunRef> {
    match crate::local::local_run(store, run_id)? {
        Some(run) => Ok(RunRef::Local(Box::new(run))),
        None => Ok(RunRef::Server(run_id.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::model::{LocalExperiment, LocalProject};
    use crate::store::{now_ms, StoredRun};

    /// A fresh, throwaway store rooted at a unique temp dir. Opened via
    /// `Store::open_at` — never by mutating `$ORX_DATA_DIR`, which is
    /// process-global and owned by the localbox lifecycle test.
    fn temp_store() -> Store {
        let dir = std::env::temp_dir().join(format!("orx-resolve-{}", uuid::Uuid::new_v4()));
        Store::open_at(dir).expect("open temp store")
    }

    fn project(id: &str) -> LocalProject {
        let now = now_ms();
        LocalProject {
            id: id.to_string(),
            name: "P".to_string(),
            slug: format!("slug-{id}"),
            github_owner: "o".to_string(),
            github_repo: "r".to_string(),
            baseline_branch: "main".to_string(),
            repo_path: "/tmp/repo".to_string(),
            run_command: None,
            paper_id: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn experiment(id: &str, project_id: &str) -> LocalExperiment {
        let now = now_ms();
        LocalExperiment {
            id: id.to_string(),
            project_id: project_id.to_string(),
            parent_experiment_id: None,
            slug: format!("exp-{id}"),
            branch_name: format!("orx/{id}"),
            title: None,
            description: None,
            run_command: "echo hi".to_string(),
            agent_status: "idle".to_string(),
            created_at: now,
            updated_at: now,
        }
    }

    fn run(id: &str, experiment_id: &str, project_id: &str) -> StoredRun {
        let now = now_ms();
        StoredRun {
            id: id.to_string(),
            experiment_id: experiment_id.to_string(),
            project_id: project_id.to_string(),
            status: "running".to_string(),
            backend_json: "{}".to_string(),
            command: "echo hi".to_string(),
            created_at: now,
            updated_at: now,
            ended_at: None,
            exit_code: None,
            commit_sha: None,
            result_markdown: None,
            cancel_requested: false,
        }
    }

    #[test]
    fn local_project_id_resolves_local() {
        let store = temp_store();
        store.create_local_project(&project("p1")).unwrap();
        match resolve_project(&store, "p1").unwrap() {
            ProjectRef::Local(p) => assert_eq!(p.id, "p1"),
            ProjectRef::Server(_) => panic!("known local project must resolve Local"),
        }
    }

    #[test]
    fn unknown_project_id_resolves_server() {
        let store = temp_store();
        match resolve_project(&store, "nope").unwrap() {
            ProjectRef::Server(id) => assert_eq!(id, "nope"),
            ProjectRef::Local(_) => panic!("unknown id must resolve Server"),
        }
    }

    #[test]
    fn local_experiment_id_resolves_local() {
        let store = temp_store();
        store.create_local_project(&project("p1")).unwrap();
        store
            .create_local_experiment(&experiment("e1", "p1"))
            .unwrap();
        match resolve_experiment(&store, "e1").unwrap() {
            ExperimentRef::Local(e) => assert_eq!(e.id, "e1"),
            ExperimentRef::Server(_) => panic!("known local experiment must resolve Local"),
        }
    }

    #[test]
    fn unknown_experiment_id_resolves_server() {
        let store = temp_store();
        match resolve_experiment(&store, "nope").unwrap() {
            ExperimentRef::Server(id) => assert_eq!(id, "nope"),
            ExperimentRef::Local(_) => panic!("unknown id must resolve Server"),
        }
    }

    #[test]
    fn server_hf_run_in_runs_table_resolves_server() {
        // Regression guard: a server-mode HF run lands in the `runs` table but
        // its experiment is NOT in `local_experiments`. Store membership alone
        // must not make it "local".
        let store = temp_store();
        store
            .upsert_run(&run("r1", "exp-not-local", "srv-project"))
            .unwrap();
        match resolve_run(&store, "r1").unwrap() {
            RunRef::Server(id) => assert_eq!(id, "r1"),
            RunRef::Local(_) => panic!("run without a local experiment must resolve Server"),
        }
    }

    #[test]
    fn run_of_local_experiment_resolves_local() {
        let store = temp_store();
        store.create_local_project(&project("p1")).unwrap();
        store
            .create_local_experiment(&experiment("e1", "p1"))
            .unwrap();
        store.upsert_run(&run("r1", "e1", "p1")).unwrap();
        match resolve_run(&store, "r1").unwrap() {
            RunRef::Local(r) => assert_eq!(r.id, "r1"),
            RunRef::Server(_) => panic!("run of a local experiment must resolve Local"),
        }
    }
}
