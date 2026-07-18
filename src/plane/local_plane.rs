//! `LocalPlane` — the local-store control plane (`store` + `src/local`).
//!
//! Holds the owning `Store` and, when the resolver already fetched it, the
//! resolved `LocalProject` / `LocalExperiment` row (so the verb needs no second
//! lookup). The verb bodies are the former `commands::{runs,logs,project,exp,
//! create_experiment,report}` local fns, moved here almost verbatim.
//!
//! Verbs that are server-only today return the SAME error the command returned:
//! `set_experiment_command` → `local::unsupported("exp cmd")`; `report` → the
//! files-dir guidance. Byte-identical.

use std::collections::HashMap;
use std::io::{Read as _, Seek as _};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use super::server_plane::sleep_until_or_timeout;
use super::{
    ControlPlane, CreateExperimentSpec, DescInput, LogRequest, ProjectEdit, Run, RunListing, RunLog,
};
use crate::error::{anyhow, Result};
use crate::local::model::{LocalExperiment, LocalProject};
use crate::store::{log_path, Store};
use crate::{ExpRunArgs, ReportCommand};

/// The local-store plane. `project`/`experiment` carry the row the resolver
/// already fetched (present for project-/experiment-keyed commands); `id` is the
/// resolved project/experiment/run id for the run-keyed and re-lookup paths.
pub struct LocalPlane {
    pub(super) store: Store,
    pub(super) project: Option<LocalProject>,
    pub(super) experiment: Option<LocalExperiment>,
    pub(super) id: String,
}

impl LocalPlane {
    /// The resolved project row, or an error if this plane wasn't built from a
    /// project id. In practice unreachable — the resolvers set `project` for
    /// every project-keyed command — but avoids an `unwrap`.
    fn project(&self) -> Result<&LocalProject> {
        self.project
            .as_ref()
            .ok_or_else(|| anyhow!("internal: local plane missing its project row"))
    }

    /// The resolved experiment row, analogous to `project`.
    fn experiment(&self) -> Result<&LocalExperiment> {
        self.experiment
            .as_ref()
            .ok_or_else(|| anyhow!("internal: local plane missing its experiment row"))
    }
}

/// Default byte window for local head/tail reads without `--bytes`.
const LOCAL_DEFAULT_BYTES: i64 = 64 * 1024;

#[async_trait(?Send)]
impl ControlPlane for LocalPlane {
    fn is_local(&self) -> bool {
        true
    }

    // --- runs -------------------------------------------------------------

    async fn list_runs(&self) -> Result<RunListing> {
        let store = &self.store;
        let project_id = &self.id;
        let titles: HashMap<String, String> = store
            .list_experiments_by_project(project_id)?
            .into_iter()
            .map(|e| (e.id.clone(), e.display_name().to_string()))
            .collect();

        // Already newest-first (store orders by created_at DESC).
        let runs = store.list_runs_by_project(project_id)?;
        let runs: Vec<Run> = runs.iter().map(Run::from).collect();
        Ok(RunListing { runs, titles })
    }

    // --- logs -------------------------------------------------------------

    async fn read_log(&self, req: LogRequest) -> Result<RunLog> {
        let run_id = &self.id;
        let path = log_path(run_id);
        let total = match std::fs::metadata(&path) {
            Ok(m) => m.len() as i64,
            Err(_) => {
                return Ok(RunLog {
                    content: Vec::new(),
                    start_byte: 0,
                    end_byte: 0,
                    total_bytes: 0,
                    source: "local file".to_string(),
                    truncated_before: false,
                    truncated_after: false,
                    missing_local: true,
                });
            }
        };

        let max = req.max_bytes.unwrap_or(LOCAL_DEFAULT_BYTES).max(0);
        let (start, end) = match req.mode.as_str() {
            "range" => (
                req.start_byte.unwrap_or(0).clamp(0, total),
                req.end_byte.unwrap_or(total).clamp(0, total),
            ),
            "head" => (0, max.min(total)),
            _ => ((total - max).max(0), total),
        };

        let mut content = Vec::new();
        if end > start {
            let mut f = std::fs::File::open(&path)?;
            f.seek(std::io::SeekFrom::Start(start as u64))?;
            f.take((end - start) as u64).read_to_end(&mut content)?;
        }

        Ok(RunLog {
            content,
            start_byte: start,
            end_byte: end,
            total_bytes: total,
            source: "local file".to_string(),
            truncated_before: start > 0,
            truncated_after: end < total,
            missing_local: false,
        })
    }

    // --- project ----------------------------------------------------------

    async fn view_project(&self) -> Result<()> {
        let store = &self.store;
        let project = self.project()?;
        println!("{} (local)", project.name);
        println!("  id:      {}", project.id);
        println!(
            "  repo:    {}/{} (baseline branch: {})",
            project.github_owner, project.github_repo, project.baseline_branch
        );
        println!("  clone:   {}", project.repo_path);
        match project
            .run_command
            .as_deref()
            .filter(|c| !c.trim().is_empty())
        {
            Some(cmd) => println!("  command: {}", cmd),
            None => println!(
                "  command: — (not set — `orx project edit {} --run-command '<cmd>'`)",
                project.id
            ),
        }

        let experiments = store.list_experiments_by_project(&project.id)?;
        println!("\nExperiments");
        if experiments.is_empty() {
            println!("  (none)");
        } else {
            for e in &experiments {
                let root = if e.parent_experiment_id.is_none() {
                    " [root]"
                } else {
                    ""
                };
                println!(
                    "  {}  {}{}  ({})",
                    e.id,
                    e.display_name(),
                    root,
                    e.branch_name
                );
            }
        }
        Ok(())
    }

    async fn edit_project(&self, edit: ProjectEdit) -> Result<()> {
        // Local projects support --name and --run-command only; the command
        // validated the combination, but keep the guard for the direct path.
        if edit.description.is_some() || edit.description_stdin || edit.public || edit.private {
            return Err(anyhow!(
                "Local projects support --name and --run-command only."
            ));
        }
        let mut project = self.project()?.clone();
        let name = edit.name;
        let run_command = edit.run_command;
        if name.is_none() && run_command.is_none() {
            return Err(anyhow!(
                "Nothing to change. Pass at least one of --name or --run-command."
            ));
        }
        if let Some(name) = name {
            if name.trim().is_empty() {
                return Err(anyhow!("--name cannot be empty."));
            }
            project.name = name.trim().to_string();
        }
        if let Some(cmd) = run_command {
            project.run_command = Some(cmd).filter(|c| !c.trim().is_empty());
        }
        self.store.update_local_project(&project)?;

        println!("\u{2713} Project updated.");
        println!("  id:      {}", project.id);
        println!("  name:    {}", project.name);
        match project.run_command.as_deref() {
            Some(cmd) => println!("  command: {}", cmd),
            None => println!("  command: — (empty)"),
        }
        Ok(())
    }

    // --- experiment -------------------------------------------------------

    async fn experiment_status(&self) -> Result<()> {
        let store = &self.store;
        let exp = self.experiment()?;
        println!("{}  ({})  [local]", exp.display_name(), exp.agent_status);
        println!("  id:       {}", exp.id);
        println!("  branch:   {}", exp.branch_name);
        match &exp.parent_experiment_id {
            Some(parent_id) => match store.get_local_experiment(parent_id)? {
                Some(parent) => {
                    println!("  parent:   {} (branch {})", parent_id, parent.branch_name)
                }
                None => println!("  parent:   {}", parent_id),
            },
            None => println!("  parent:   — (root experiment)"),
        }
        if exp.run_command.is_empty() {
            println!("  command:  — (not set)");
        } else {
            println!("  command:  {}", exp.run_command);
        }

        match store.latest_run_for_experiment(&exp.id)? {
            Some(r) => {
                let run = Run::from(&r);
                let commit = run
                    .commit_sha
                    .as_deref()
                    .map(|s| s.chars().take(7).collect::<String>())
                    .unwrap_or_else(|| "—".to_string());
                println!(
                    "  last run: {} ({}, commit {}, ran {}, updated {})",
                    run.id,
                    run.status,
                    commit,
                    crate::output::format_duration(run.duration_secs),
                    run.updated_display
                );
                if let Some(detail) = run.failure_detail() {
                    println!("  {detail}");
                }
                if let Some(sha) = &run.commit_sha {
                    println!("  commit:   {}", sha);
                }
            }
            None => println!("  last run: — (never run)"),
        }
        Ok(())
    }

    async fn experiment_desc(&self, set: Option<String>, stdin: bool) -> Result<()> {
        let input = DescInput::resolve(set, stdin).await?;
        let mut exp = self.experiment()?.clone();
        match input {
            DescInput::Set(description) => {
                exp.description = Some(description);
                self.store.update_local_experiment(&exp)?;
                println!("\u{2713} Description saved.");
            }
            DescInput::Get => match exp.description.as_deref().filter(|d| !d.trim().is_empty()) {
                Some(d) => println!("{d}"),
                None => eprintln!(
                    "No description set. Add one with `orx exp desc {} --set \"…\"` \
                     or pipe a file: `cat notes.md | orx exp desc {} --stdin`.",
                    exp.id, exp.id
                ),
            },
        }
        Ok(())
    }

    async fn set_experiment_command(&self, _command: Option<String>) -> Result<()> {
        Err(crate::local::unsupported("exp cmd"))
    }

    async fn launch(&self, mut args: ExpRunArgs) -> Result<()> {
        // Fill backend/flavor from the persisted default (Settings → Compute)
        // BEFORE the flag validations below, so e.g. `--host box1` with a default
        // of `ssh` is a valid launch, and before `backend_label` is captured, so
        // telemetry records the resolved backend.
        crate::local::apply_compute_default(&mut args.backend, &mut args.flavor);
        if args.manifest.is_some() && args.backend.as_deref() != Some("k8s") {
            return Err(anyhow!("--manifest only applies with --backend k8s."));
        }
        if args.host.is_some() && !matches!(args.backend.as_deref(), Some("ssh") | Some("slurm")) {
            return Err(anyhow!("--host only applies with --backend ssh or slurm."));
        }
        if args.org.is_some() && args.backend.as_deref() != Some("openresearch") {
            return Err(anyhow!("--org only applies with --backend openresearch."));
        }
        // Coarse backend label for analytics; the backend name is already an
        // enum, never user data. Recorded before the (borrowing) dispatch below.
        let backend_label = args.backend.clone();
        let result = match args.backend.as_deref() {
            Some("hf") => crate::local::hf::launch_local_hf(&args).await,
            Some("modal") => crate::local::modal::launch_local_modal(&args).await,
            Some("k8s") => crate::local::k8s::launch_local_k8s(&args).await,
            Some("ssh") => crate::local::ssh::launch_local_ssh(&args).await,
            Some("slurm") => crate::local::slurm::launch_local_slurm(&args).await,
            Some("openresearch") => {
                crate::local::openresearch::launch_local_openresearch(&args).await
            }
            Some("local") => crate::local::localrun::launch_local_run(&args).await,
            Some(other) => Err(anyhow!(
                "Unknown --backend '{}'. Local experiments support: hf (Hugging Face Jobs), \
                 modal (Modal serverless GPUs), k8s (your Kubernetes cluster), ssh (your own box), \
                 slurm (your Slurm cluster), openresearch (an ephemeral OpenResearch box), \
                 local (this machine).",
                other
            )),
            None => Err(anyhow!(
                "No --backend given and no default compute target is set. \
                 Set a default in the dashboard (`orx up` → Settings → Compute → Make default), \
                 or pass one per launch: \
                 `--backend hf --flavor <flavor>` (e.g. --flavor a10g-small), \
                 `--backend modal --flavor <flavor>` (e.g. --flavor a10g), \
                 `--backend k8s` (runs the manifest committed on the branch — \
                 default .orx/k8s.yaml, or --manifest <path>), \
                 `--backend ssh --host <alias>` (an ~/.ssh/config alias), \
                 `--backend slurm [--host <alias>] [--flavor h100:2]` (your Slurm cluster), \
                 `--backend openresearch --flavor <shape>` (an ephemeral OpenResearch box, \
                 e.g. --flavor h100_sxm or cpu5c; needs `orx login`), \
                 or `--backend local` (a detached process on this machine)."
            )),
        };
        // Key event, fired only on a successful launch. Coarse backend only.
        // `backend_label` is always `Some(<known backend>)` here — every arm that
        // yields `Ok` matched a `Some("hf"|"modal"|...)`; `None`/unknown arms
        // return `Err`. The `"unknown"` fallback is unreachable defense.
        if result.is_ok() {
            let target = backend_label.as_deref().unwrap_or("unknown");
            crate::telemetry::capture_experiment_started("run", true, Some(target));
        }
        result
    }

    async fn cancel(&self) -> Result<()> {
        let store = &self.store;
        let exp = self.experiment()?;
        let in_flight: Vec<_> = store
            .list_runs_by_experiment(&exp.id)?
            .into_iter()
            .filter(|r| !crate::local::is_terminal(&r.status))
            .collect();
        if in_flight.is_empty() {
            return Err(anyhow!("No run in flight for this experiment."));
        }
        for r in &in_flight {
            store.set_cancel_requested(&r.id, true)?;
            println!("\u{2713} Cancel requested for run {}.", r.id);
        }
        Ok(())
    }

    async fn wait_experiment(&self, interval: Duration, deadline: Instant) -> Result<()> {
        let store = &self.store;
        let exp_id = &self.id;
        let mut last_status: Option<String> = None;
        loop {
            match store.latest_run_for_experiment(exp_id)? {
                None => {
                    if last_status.is_none() {
                        eprintln!("No run yet for this experiment — waiting for one to start…");
                        last_status = Some(String::new());
                    }
                }
                Some(r) => {
                    if last_status.as_deref() != Some(r.status.as_str()) {
                        eprintln!("{}  {}", r.id, r.status);
                        last_status = Some(r.status.clone());
                    }
                    if crate::local::is_terminal(&r.status) {
                        let run = Run::from(&r);
                        println!("{} {}", run.id, run.status);
                        if let Some(detail) = run.failure_detail() {
                            eprintln!("{detail}");
                        }
                        return Ok(());
                    }
                }
            }
            sleep_until_or_timeout(interval, deadline).await?;
        }
    }

    async fn wait_project(&self, interval: Duration, deadline: Instant) -> Result<()> {
        let store = &self.store;
        let project_id = &self.id;
        let snapshot: HashMap<String, String> = store
            .list_runs_by_project(project_id)?
            .into_iter()
            .map(|r| (r.id, r.status))
            .collect();
        let in_flight = snapshot
            .values()
            .filter(|s| !crate::local::is_terminal(s))
            .count();

        if in_flight == 0 {
            eprintln!(
                "No runs in flight in this project ({} run(s), all terminal).",
                snapshot.len()
            );
            println!("drained: no runs in flight");
            return Ok(());
        }

        eprintln!(
            "Watching {} run(s) in project ({} in flight) — returning on the first completion…",
            snapshot.len(),
            in_flight
        );

        loop {
            sleep_until_or_timeout(interval, deadline).await?;

            let current = store.list_runs_by_project(project_id)?;
            let mut completed: Vec<(String, Option<String>)> = Vec::new();
            for r in &current {
                if !crate::local::is_terminal(&r.status) {
                    continue;
                }
                let line = match snapshot.get(&r.id) {
                    Some(prev) if crate::local::is_terminal(prev) => continue,
                    Some(prev) => format!("{} {} -> {}", r.id, prev, r.status),
                    None => format!("{} {} (new)", r.id, r.status),
                };
                completed.push((line, Run::from(r).failure_detail()));
            }
            if !completed.is_empty() {
                for (line, detail) in &completed {
                    println!("{line}");
                    if let Some(detail) = detail {
                        eprintln!("{detail}");
                    }
                }
                return Ok(());
            }
        }
    }

    // --- create-experiment ------------------------------------------------

    async fn create_experiment(&self, spec: CreateExperimentSpec) -> Result<()> {
        let store = &self.store;
        let project = self.project()?;
        let CreateExperimentSpec {
            title,
            parent,
            baseline,
            description,
            run_command,
        } = spec;

        let mut defaulted_to_root = false;
        let parent_exp = match &parent {
            Some(parent_id) => Some(store.get_local_experiment(parent_id)?.ok_or_else(|| {
                anyhow!(
                    "Parent experiment {} not found in the local store. \
                     See the dashboard, or omit --parent to branch off the project root.",
                    parent_id
                )
            })?),
            None if baseline => None,
            None => {
                let root = crate::local::experiments::project_root(store, &project.id)?;
                defaulted_to_root = root.is_some();
                root
            }
        };
        let kind = if parent_exp.is_some() {
            "child"
        } else {
            "baseline"
        };

        let experiment = crate::local::experiments::create_experiment(
            store,
            project,
            parent_exp.as_ref(),
            None,
            Some(title),
            description,
            run_command,
        )?;

        println!("\u{2713} Created local {} experiment", kind);
        if defaulted_to_root {
            let root = parent_exp.as_ref().unwrap();
            println!("  parent:  {} (project root, defaulted)", root.id);
        }
        if let Some(warning) = parent_exp
            .as_ref()
            .and_then(|p| crate::local::experiments::legacy_root_warning(project, p))
        {
            eprintln!("  {warning}");
        }
        println!("  id:      {}", experiment.id);
        println!("  title:   {}", experiment.display_name());
        println!("  slug:    {}", experiment.slug);
        println!("  branch:  {}", experiment.branch_name);
        if experiment.run_command.is_empty() {
            println!(
                "  command: — (none inherited — set one with `orx project edit {} --run-command '<cmd>'`)",
                project.id
            );
        } else {
            println!("  command: {}", experiment.run_command);
        }
        println!();
        println!("To edit it, check out the branch in the project's local clone:");
        println!("  cd {}", project.repo_path);
        println!(
            "  git fetch origin && git checkout {}",
            experiment.branch_name
        );
        println!("  # …edit, then…");
        println!(
            "  git commit -am \"<msg>\" && git push -u origin {}",
            experiment.branch_name
        );
        Ok(())
    }

    // --- reports (local has no report registry) ---------------------------

    async fn report(&self, _cmd: ReportCommand) -> Result<()> {
        // Local projects have no report registry or upload step — the files dir
        // on disk is the whole feature. Point there instead of pretending to
        // upload. Byte-identical to the former `report::local_guidance`.
        let project = self.project()?;
        let dir = crate::local::files::ensure_dir(project)?;
        Err(anyhow!(
            "`orx report` is cloud-only. Local projects have no upload step: write the report \
             folder (report.md + images/) straight into the project's files directory,\n  {}\n\
             Everything in that directory shows up in the dashboard's Files tab.",
            dir.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, throwaway store rooted at a unique temp dir (never mutates the
    /// process-global `$ORX_DATA_DIR`).
    fn temp_store() -> Store {
        let dir = std::env::temp_dir().join(format!("orx-plane-{}", uuid::Uuid::new_v4()));
        Store::open_at(dir).expect("open temp store")
    }

    fn empty_local_plane() -> LocalPlane {
        LocalPlane {
            store: temp_store(),
            project: None,
            experiment: None,
            id: "e1".to_string(),
        }
    }

    #[tokio::test]
    async fn set_experiment_command_is_unsupported_locally() {
        // The server-only verb must reproduce `local::unsupported("exp cmd")`
        // byte-for-byte (the old `exp cmd` local arm's error).
        let plane = empty_local_plane();
        let err = plane
            .set_experiment_command(Some("echo hi".to_string()))
            .await
            .expect_err("exp cmd must be unsupported on a local experiment");
        assert_eq!(
            err.to_string(),
            crate::local::unsupported("exp cmd").to_string()
        );
    }
}
