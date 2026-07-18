//! `ServerPlane` — the cloud-api control plane, wrapping `client.rs`.
//!
//! Constructed only on the server arm of a resolved command, so it fetches
//! credentials up front (`ServerPlane::connect` → `require_credentials`) exactly
//! as the old server bodies did on entry. The verb bodies below are the former
//! `commands::{runs,logs,project,exp,create_experiment,report}` server fns moved
//! here almost verbatim; only signatures/`self` were adapted.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::AsyncReadExt;

use super::{
    ControlPlane, CreateExperimentSpec, DescInput, LogRequest, ProjectEdit, Run, RunListing, RunLog,
};
use crate::client::{
    cancel_experiment_run, create_baseline_experiment, create_child_experiment,
    create_external_run, create_report, download_report_file, find_project, get_experiment,
    get_project, get_report, list_experiments, list_reports, list_runs, read_run_log,
    start_experiment_run, update_experiment, update_project, upload_to_presigned,
    CreateBaselineExperimentBody, CreateChildBody, CreateReportBody, RunTarget,
    UpdateExperimentBody, UpdateProjectBody,
};
use crate::commands::experiments::print_tree;
use crate::config::Credentials;
use crate::error::{anyhow, require_credentials, Result};
use crate::jobs::{huggingface as hf, BackendDescriptor};
use crate::output::format_duration;
use crate::store::{now_ms, Store, StoredRun};
use crate::{ExpRunArgs, ReportCommand};

/// The cloud-api plane. `id` is the project/experiment/run id the command
/// resolved to `Server`; `creds` are fetched at construction.
pub struct ServerPlane {
    id: String,
    creds: Credentials,
}

impl ServerPlane {
    /// Fetch credentials and finish construction. Reached only on server arms —
    /// same as the old code, so a local-only user who never logged in never hits
    /// this.
    async fn connect(id: String) -> ServerPlane {
        let creds = require_credentials().await;
        ServerPlane { id, creds }
    }
}

/// A not-yet-connected server plane. The resolvers are sync (and must run their
/// login-independent guards — e.g. the `--run-command`-on-server-child refusal —
/// BEFORE any `require_credentials`, matching the old arm ordering), so they box
/// this. The real `ServerPlane` is built by connecting (fetching credentials) on
/// the verb call, which is exactly where the old server bodies called
/// `require_credentials`. This keeps `require_credentials` off the local path and
/// off command entry, and preserves the guard-before-login order.
pub struct ServerPlaceholder {
    pub(super) id: String,
}

#[async_trait(?Send)]
impl ControlPlane for ServerPlaceholder {
    fn is_local(&self) -> bool {
        false
    }

    async fn view_project(&self) -> Result<()> {
        ServerPlane::connect(self.id.clone())
            .await
            .view_project()
            .await
    }
    async fn edit_project(&self, edit: ProjectEdit) -> Result<()> {
        // The server project PATCH carries no run command field — refuse before
        // even asking for credentials.
        if edit.run_command.is_some() {
            return Err(anyhow!(
                "--run-command is supported for local projects only. For server \
                 projects, set it per experiment with `orx exp cmd <expId> --set '<cmd>'`."
            ));
        }
        ServerPlane::connect(self.id.clone())
            .await
            .edit_project(edit)
            .await
    }
    async fn list_runs(&self) -> Result<RunListing> {
        ServerPlane::connect(self.id.clone())
            .await
            .list_runs()
            .await
    }
    async fn read_log(&self, req: LogRequest) -> Result<RunLog> {
        ServerPlane::connect(self.id.clone())
            .await
            .read_log(req)
            .await
    }
    async fn experiment_status(&self) -> Result<()> {
        ServerPlane::connect(self.id.clone())
            .await
            .experiment_status()
            .await
    }
    async fn experiment_desc(&self, set: Option<String>, stdin: bool) -> Result<()> {
        ServerPlane::connect(self.id.clone())
            .await
            .experiment_desc(set, stdin)
            .await
    }
    async fn set_experiment_command(&self, command: Option<String>) -> Result<()> {
        ServerPlane::connect(self.id.clone())
            .await
            .set_experiment_command(command)
            .await
    }
    async fn launch(&self, args: ExpRunArgs) -> Result<()> {
        ServerPlane::connect(self.id.clone())
            .await
            .launch(args)
            .await
    }
    async fn cancel(&self) -> Result<()> {
        ServerPlane::connect(self.id.clone()).await.cancel().await
    }
    async fn wait_experiment(&self, interval: Duration, deadline: Instant) -> Result<()> {
        ServerPlane::connect(self.id.clone())
            .await
            .wait_experiment(interval, deadline)
            .await
    }
    async fn wait_project(&self, interval: Duration, deadline: Instant) -> Result<()> {
        ServerPlane::connect(self.id.clone())
            .await
            .wait_project(interval, deadline)
            .await
    }
    async fn create_experiment(&self, spec: CreateExperimentSpec) -> Result<()> {
        // The server child-create API carries no run command field — refuse
        // rather than silently drop it. (The baseline create does accept one.)
        // Refuse before asking for credentials, matching the old server body.
        if spec.run_command.is_some() && spec.parent.is_some() {
            return Err(anyhow!(
                "--run-command is supported for local projects and server baselines \
                 only. For server child experiments, set it after creation with \
                 `orx exp cmd <expId> --set '<cmd>'`."
            ));
        }
        ServerPlane::connect(self.id.clone())
            .await
            .create_experiment(spec)
            .await
    }
    async fn report(&self, cmd: ReportCommand) -> Result<()> {
        ServerPlane::connect(self.id.clone())
            .await
            .report(cmd)
            .await
    }
}

#[async_trait(?Send)]
impl ControlPlane for ServerPlane {
    fn is_local(&self) -> bool {
        false
    }

    // --- runs -------------------------------------------------------------

    async fn list_runs(&self) -> Result<RunListing> {
        let creds = &self.creds;
        let project_id = &self.id;

        // Fetch experiments too, so we can label each run with its experiment
        // title rather than a bare id. Both requests run concurrently.
        let (runs_res, experiments_res) = tokio::join!(
            list_runs(creds, project_id),
            list_experiments(creds, project_id)
        );
        let runs = runs_res?.runs;
        let experiments = experiments_res?.experiments;

        let titles: HashMap<String, String> =
            experiments.into_iter().map(|e| (e.id, e.title)).collect();

        // Run ids are UUIDv7 — lexicographic sort is chronological. Newest first.
        let mut runs: Vec<Run> = runs.into_iter().map(Run::from).collect();
        runs.sort_by(|a, b| b.id.cmp(&a.id));

        Ok(RunListing { runs, titles })
    }

    // --- logs -------------------------------------------------------------

    async fn read_log(&self, req: LogRequest) -> Result<RunLog> {
        let log = read_run_log(
            &self.creds,
            &self.id,
            Some(&req.mode),
            req.max_bytes,
            req.start_byte,
            req.end_byte,
        )
        .await?;
        Ok(RunLog {
            content: log.content.into_bytes(),
            start_byte: log.start_byte,
            end_byte: log.end_byte,
            total_bytes: log.total_bytes,
            source: log.source,
            truncated_before: log.truncated_before,
            truncated_after: log.truncated_after,
            missing_local: false,
        })
    }

    // --- project ----------------------------------------------------------

    async fn view_project(&self) -> Result<()> {
        let creds = &self.creds;
        let project_id = &self.id;
        let project = get_project(creds, project_id).await?.project;

        println!("{}", project.name);
        println!("  id:     {}", project.id);
        if !project.github_owner.is_empty() {
            println!("  repo:   {}/{}", project.github_owner, project.github_repo);
        }
        println!(
            "  access: {}",
            if project.is_public {
                "public"
            } else {
                "private"
            }
        );
        if !project.description.is_empty() {
            println!("  about:  {}", project.description);
        }
        if let Some(q) = project
            .example_question
            .as_deref()
            .filter(|q| !q.is_empty())
        {
            println!("  ask:    {}", q);
        }

        let experiments = list_experiments(creds, project_id).await?.experiments;
        println!("\nExperiments");
        if experiments.is_empty() {
            println!("  (none)");
        } else {
            print_tree(&experiments);
        }

        let reports = list_reports(creds, project_id).await?.reports;
        println!("\nReports");
        if reports.is_empty() {
            println!("  (none)");
        } else {
            for r in &reports {
                println!("  {}  {}  ({})", r.id, r.title, r.created_at);
            }
            println!("\nRead one with: orx report show {} <reportId>", project_id);
        }
        Ok(())
    }

    async fn edit_project(&self, edit: ProjectEdit) -> Result<()> {
        let creds = &self.creds;
        let project_id = &self.id;

        // `--description` and `--description-stdin` are mutually exclusive; either
        // present means "overwrite the description".
        let description = match (edit.description, edit.description_stdin) {
            (Some(_), true) => {
                return Err(anyhow!(
                    "Pass either --description or --description-stdin, not both."
                ))
            }
            (Some(text), false) => Some(text),
            (None, true) => {
                let mut buf = String::new();
                tokio::io::stdin().read_to_string(&mut buf).await?;
                Some(buf)
            }
            (None, false) => None,
        };

        // `--public` / `--private` map to the `isPublic` flag; clap's
        // `conflicts_with` already rejects passing both. Neither flag leaves
        // visibility untouched (`None`).
        let is_public = match (edit.public, edit.private) {
            (true, false) => Some(true),
            (false, true) => Some(false),
            _ => None,
        };

        if edit.name.is_none() && description.is_none() && is_public.is_none() {
            return Err(anyhow!(
                "Nothing to change. Pass at least one of --name, --description \
                 (or --description-stdin), --public, or --private."
            ));
        }

        let res = update_project(
            creds,
            project_id,
            &UpdateProjectBody {
                name: edit.name,
                description,
                is_public,
            },
        )
        .await?;
        let project = res.project;

        println!("\u{2713} Project updated.");
        println!("  id:          {}", project.id);
        println!("  name:        {}", project.name);
        println!(
            "  access:      {}",
            if project.is_public {
                "public"
            } else {
                "private"
            }
        );
        if project.description.is_empty() {
            println!("  description: — (empty)");
        } else {
            println!("  description: {}", project.description);
        }
        Ok(())
    }

    // --- experiment -------------------------------------------------------

    async fn experiment_status(&self) -> Result<()> {
        let creds = &self.creds;
        let exp_id = &self.id;
        let res = get_experiment(creds, exp_id).await?;
        let exp = res.experiment;

        // Parent branch (the diff base). Best-effort: a failed parent fetch
        // degrades to printing the id alone, never fails the status command.
        let parent_branch: Option<String> = match &exp.parent_experiment_id {
            Some(parent_id) => get_experiment(creds, parent_id)
                .await
                .ok()
                .map(|p| p.experiment.branch_name),
            None => None,
        };

        println!("{}  ({})", exp.title, exp.agent_status);
        println!("  id:       {}", exp.id);
        println!("  branch:   {}", exp.branch_name);
        match (&exp.parent_experiment_id, &parent_branch) {
            (Some(id), Some(branch)) => println!("  parent:   {} (branch {})", id, branch),
            (Some(id), None) => println!("  parent:   {}", id),
            (None, _) => println!("  parent:   — (root experiment)"),
        }
        match &exp.sandbox_id {
            Some(sb) => println!("  sandbox:  {}", sb),
            None => println!("  sandbox:  — (none linked)"),
        }
        if exp.run_command.is_empty() {
            println!(
                "  command:  — (not set — `orx exp cmd {} --set \"…\"`)",
                exp.id
            );
        } else {
            println!("  command:  {}", exp.run_command);
        }

        let mut full_sha: Option<String> = None;
        match res.latest_run {
            Some(r) => {
                let run = Run::from(r);
                let commit = run
                    .commit_sha
                    .as_ref()
                    .map(|s| s.chars().take(7).collect::<String>())
                    .unwrap_or_else(|| "—".to_string());
                println!(
                    "  last run: {} ({}, commit {}, ran {}, updated {})",
                    run.id,
                    run.status,
                    commit,
                    format_duration(run.duration_secs),
                    run.updated_display
                );
                if let Some(detail) = run.failure_detail() {
                    println!("  {detail}");
                }
                if let Some(sha) = run.commit_sha {
                    println!("  commit:   {}", sha);
                    full_sha = Some(sha);
                }
            }
            None => println!("  last run: — (never run)"),
        }

        // Local diff recipe — only when there's both a base (parent branch) and
        // a head (run commit) to compare. Owner/repo lookup is best-effort too:
        // on failure print placeholders the caller can fill from `orx projects`.
        if let (Some(branch), Some(sha)) = (parent_branch, full_sha) {
            let repo_path = match find_project(creds, &exp.project_id).await {
                Ok(Some(p)) if !p.github_owner.is_empty() && !p.github_repo.is_empty() => {
                    format!("{}/{}", p.github_owner, p.github_repo)
                }
                _ => "<owner>/<repo>".to_string(),
            };
            let dir = format!("~/.cache/openresearch/repos/{}", repo_path);
            println!();
            println!("To see what this run changed vs. its parent, using your local clone (cloned on first use):");
            if repo_path == "<owner>/<repo>" {
                println!("  # owner/repo from `orx projects`");
            }
            println!(
                "  [ -d {} ] || git clone https://github.com/{} {}",
                dir, repo_path, dir
            );
            println!("  git -C {} fetch origin", dir);
            println!("  git -C {} diff origin/{}...{}", dir, branch, sha);
        }

        Ok(())
    }

    async fn experiment_desc(&self, set: Option<String>, stdin: bool) -> Result<()> {
        // Resolve after connect: `--stdin` must not be consumed before the
        // login check (pre-trait ordering).
        let input = DescInput::resolve(set, stdin).await?;
        let creds = &self.creds;
        let exp_id = &self.id;
        match input {
            // Write path: overwrite the whole description.
            DescInput::Set(description) => {
                update_experiment(
                    creds,
                    exp_id,
                    &UpdateExperimentBody {
                        description: Some(description),
                        ..Default::default()
                    },
                )
                .await?;
                println!("\u{2713} Description saved.");
            }
            // Read path: print to stdout (pipe-friendly), or hint when empty.
            DescInput::Get => {
                let res = get_experiment(creds, exp_id).await?;
                if res.experiment.description.is_empty() {
                    eprintln!(
                        "No description set. Add one with `orx exp desc {} --set \"…\"` \
                         or pipe a file: `cat notes.md | orx exp desc {} --stdin`.",
                        exp_id, exp_id
                    );
                } else {
                    println!("{}", res.experiment.description);
                }
            }
        }
        Ok(())
    }

    async fn set_experiment_command(&self, command: Option<String>) -> Result<()> {
        let creds = &self.creds;
        let exp_id = &self.id;
        match command {
            Some(command) => {
                let res = update_experiment(
                    creds,
                    exp_id,
                    &UpdateExperimentBody {
                        run_command: Some(command),
                        ..Default::default()
                    },
                )
                .await?;
                println!("\u{2713} Run command set:");
                println!("  {}", res.experiment.run_command);
            }
            None => {
                let res = get_experiment(creds, exp_id).await?;
                if res.experiment.run_command.is_empty() {
                    println!(
                        "No run command set. Set one with `orx exp cmd {} --set \"…\"`.",
                        exp_id
                    );
                } else {
                    println!("{}", res.experiment.run_command);
                }
            }
        }
        Ok(())
    }

    async fn launch(&self, args: ExpRunArgs) -> Result<()> {
        self.launch_impl(args).await
    }

    async fn cancel(&self) -> Result<()> {
        cancel_experiment_run(&self.creds, &self.id).await?;
        println!("\u{2713} Run cancelled.");
        Ok(())
    }

    async fn wait_experiment(&self, interval: Duration, deadline: Instant) -> Result<()> {
        let creds = &self.creds;
        let exp_id = &self.id;
        let mut last_status: Option<String> = None;
        loop {
            let res = get_experiment(creds, exp_id).await?;
            match res.latest_run {
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
                        let run = Run::from(r);
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
        let creds = &self.creds;
        let project_id = &self.id;
        let snapshot: HashMap<String, String> = list_runs(creds, project_id)
            .await?
            .runs
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

            let current = list_runs(creds, project_id).await?.runs;
            let mut completed: Vec<(String, Option<String>)> = Vec::new();
            for r in current {
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
        let creds = &self.creds;
        let project_id = &self.id;
        let CreateExperimentSpec {
            title,
            parent,
            description,
            run_command,
            baseline: _,
        } = spec;

        let experiment: crate::client::Experiment;
        let kind: String;
        if let Some(parent) = parent {
            let envelope = create_child_experiment(
                creds,
                project_id,
                &CreateChildBody {
                    title,
                    description,
                    parent_experiment_id: parent,
                },
            )
            .await?;
            experiment = envelope.experiment;
            kind = "child".to_string();
        } else {
            // Baseline on the project's already-bound GitHub repo. The server
            // branches `orx/<slug>` off the branch picked at project creation
            // (the repo's default unless one was chosen).
            let envelope = create_baseline_experiment(
                creds,
                project_id,
                &CreateBaselineExperimentBody {
                    title: Some(title),
                    description,
                    run_command,
                },
            )
            .await?;
            experiment = envelope.experiment;
            kind = "baseline".to_string();
        }

        println!("\u{2713} Created {} experiment", kind);
        println!("  id:     {}", experiment.id);
        println!("  title:  {}", experiment.title);
        println!("  slug:   {}", experiment.slug);
        println!("  branch: {}", experiment.branch_name);
        println!();
        println!("To edit it, check out the branch in your local clone of the project's repo:");
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

    // --- reports ----------------------------------------------------------

    async fn report(&self, cmd: ReportCommand) -> Result<()> {
        // The variants carry the project id the command already resolved this
        // plane from — `self.id` is that same id, so it is the single source
        // here and the embedded copies are ignored.
        match cmd {
            ReportCommand::Upload { folder, title, .. } => {
                self.report_upload(&self.id, &folder, title).await
            }
            ReportCommand::List { .. } => self.report_list(&self.id).await,
            ReportCommand::Show { report, .. } => self.report_show(&self.id, &report).await,
            ReportCommand::Download { report, dir, .. } => {
                self.report_download(&self.id, &report, &dir).await
            }
        }
    }
}

impl ServerPlane {
    /// The managed/external launch dispatcher — the former `exp::launch`.
    async fn launch_impl(&self, args: ExpRunArgs) -> Result<()> {
        let creds = &self.creds;
        // External backends: orx submits and supervises the job itself; the api
        // only mirrors. Everything below this branch is the managed path.
        if args.manifest.is_some() && args.backend.as_deref() != Some("k8s") {
            return Err(anyhow!("--manifest only applies with --backend k8s."));
        }
        if args.host.is_some() && !matches!(args.backend.as_deref(), Some("ssh") | Some("slurm")) {
            return Err(anyhow!("--host only applies with --backend ssh or slurm."));
        }
        if args.org.is_some() {
            return Err(anyhow!(
                "--org only applies with --backend openresearch (local experiments); server \
                 experiments bill the project's own org."
            ));
        }
        match args.backend.as_deref() {
            Some("hf") => return self.launch_hf(args).await,
            Some("modal") => return self.launch_modal(args).await,
            Some("k8s") => {
                return Err(anyhow!(
                    "--backend k8s is supported for local experiments (`orx up`) only for now."
                ));
            }
            Some("ssh") => {
                return Err(anyhow!(
                    "--backend ssh is supported for local experiments (`orx up`) only for now."
                ));
            }
            Some("slurm") => {
                return Err(anyhow!(
                    "--backend slurm is supported for local experiments (`orx up`) only for now."
                ));
            }
            Some("openresearch") => {
                return Err(anyhow!(
                    "--backend openresearch is for local experiments (`orx up`) only. Server \
                     experiments already run on OpenResearch compute — pass --gpu/--cpu/--sandbox."
                ));
            }
            Some("local") => {
                return Err(anyhow!(
                    "--backend local is supported for local experiments (`orx up`) only."
                ));
            }
            Some(other) => {
                return Err(anyhow!(
                    "Unknown --backend '{}'. Supported: hf (Hugging Face Jobs), \
                     modal (Modal serverless GPUs), k8s/ssh/slurm/openresearch/local \
                     (local experiments only).",
                    other
                ));
            }
            None => {}
        }
        if args.flavor.is_some() || args.image.is_some() || args.timeout.is_some() {
            return Err(anyhow!(
                "--flavor/--image/--timeout only apply with an external --backend."
            ));
        }
        if args.manifest.is_some() {
            return Err(anyhow!("--manifest only applies with --backend k8s."));
        }
        // Resolve the target: exactly one of --sandbox, --gpu, or --cpu.
        let selectors = [
            args.sandbox.is_some(),
            args.gpu.is_some(),
            args.cpu.is_some(),
        ];
        let chosen = selectors.iter().filter(|x| **x).count();
        if chosen > 1 {
            return Err(anyhow!("Pass exactly one of --sandbox, --gpu, or --cpu."));
        }
        if args.provider.is_some() && args.gpu.is_none() {
            return Err(anyhow!(
                "--provider only applies with --gpu (it selects among new GPU offers)."
            ));
        }
        let target = if let Some(sandbox_id) = &args.sandbox {
            RunTarget::Existing {
                sandbox_id: sandbox_id.clone(),
            }
        } else if let Some(gpu) = &args.gpu {
            RunTarget::New {
                gpu: gpu.clone(),
                gpu_count: args.count.unwrap_or(1),
                disk_gb: args.disk.unwrap_or(100),
                // Omitted = server default (RunPod). The server validates the
                // name and 400s on an unknown provider, so no client-side check.
                provider: args.provider.clone(),
            }
        } else if let Some(cpu_flavor) = &args.cpu {
            RunTarget::NewCpu {
                cpu_flavor: cpu_flavor.clone(),
                vcpu_count: args.vcpus.unwrap_or(8),
            }
        } else {
            return Err(anyhow!(
                "Choose compute: --gpu <id> [--count N] [--disk GB], \
                 --cpu <cpu5c|cpu5g|cpu5m> [--vcpus 2|8|32], or --sandbox <id>. \
                 See `orx compute` for available GPUs."
            ));
        };

        // Friendlier than the raw API "No run command set": tell them how to fix.
        let current = get_experiment(creds, &args.exp_id).await?;
        if current.experiment.run_command.is_empty() {
            return Err(anyhow!(
                "No run command set for this experiment. Set one first with \
                 `orx exp cmd {} --set \"…\"`.",
                args.exp_id
            ));
        }

        // Coarse target label for analytics — NOT the sandbox id / gpu / flavor.
        let target_kind = match &target {
            RunTarget::Existing { .. } => "existing",
            RunTarget::New { .. } => "gpu",
            RunTarget::NewCpu { .. } => "cpu",
        };
        start_experiment_run(creds, &args.exp_id, target, args.force).await?;

        // Key event, fired only on success. Server run (not local mode here).
        crate::telemetry::capture_experiment_started("run", false, Some(target_kind));

        println!("\u{2713} Run queued.");
        println!(
            "  Follow it with `orx runs {}` and `orx logs <runId>`.",
            current.experiment.project_id
        );
        Ok(())
    }

    /// `--backend hf` — run the experiment as a Hugging Face Job on the user's
    /// own HF account. (Former `exp::launch_hf`.)
    async fn launch_hf(&self, args: ExpRunArgs) -> Result<()> {
        let creds = &self.creds;
        if args.sandbox.is_some() || args.gpu.is_some() || args.cpu.is_some() {
            return Err(anyhow!(
                "--backend hf runs on Hugging Face Jobs; drop --gpu/--cpu/--sandbox \
                 and pass --flavor instead (e.g. --flavor a10g-small)."
            ));
        }
        let flavor = args.flavor.clone().ok_or_else(|| {
            anyhow!(
                "--backend hf requires --flavor: t4-small, a10g-small/large, l4x1, \
                 l40sx1, a100-large, h200, … (cpu-basic/cpu-upgrade for CPU). \
                 Priced per minute on your Hugging Face account."
            )
        })?;
        // HF's own default is 30 minutes — a footgun for training runs, so
        // default generously and let --timeout tighten it.
        let timeout_seconds = match &args.timeout {
            Some(t) => hf::parse_timeout(t)?,
            None => 4 * 3600,
        };
        let token = hf::resolve_token()?;
        let namespace = hf::whoami(&token).await?;

        // Register first: the run must exist in the tree before compute starts,
        // and the response carries the repo/branch/command orx needs to submit.
        let mut descriptor = BackendDescriptor {
            kind: "hf_job".to_string(),
            namespace: Some(namespace.clone()),
            job_id: None,
            flavor: Some(flavor.clone()),
            image: args.image.clone(),
            url: None,
            context: None,
            manifest: None,
            resources: None,
            ssh_host: None,
            ssh_port: None,
            ssh_user: None,
            timeout_secs: None,
        };
        let created =
            create_external_run(creds, &args.exp_id, serde_json::to_value(&descriptor)?).await?;
        let run_id = created.run.id.clone();

        let image = args
            .image
            .clone()
            .unwrap_or_else(|| crate::commands::exp::default_hf_image(&flavor));
        let script = crate::commands::exp::hf_clone_script(
            &created.branch_name,
            &created.github_owner,
            &created.github_repo,
            &created.run_command,
        );

        let mut secrets = HashMap::new();
        secrets.insert("HF_TOKEN".to_string(), token.clone());
        // Clone credential precedence: explicit GITHUB_TOKEN (env, then the box's
        // synced env file) overrides; otherwise the api's repo-scoped
        // installation token flows automatically from the org's connected GitHub
        // app — a private repo needs zero extra setup beyond having connected it.
        let github_token = std::env::var("GITHUB_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty())
            .or_else(|| crate::config::synced_env_var("GITHUB_TOKEN"))
            .or_else(|| created.github_token.clone());
        if let Some(gh) = github_token {
            secrets.insert("GITHUB_TOKEN".to_string(), gh);
        }
        let mut labels = HashMap::new();
        labels.insert("or_run".to_string(), run_id.clone());
        labels.insert("or_experiment".to_string(), args.exp_id.clone());
        labels.insert("or_project".to_string(), created.project_id.clone());

        let job = hf::run_job(
            &token,
            &namespace,
            &hf::JobSubmission {
                command: vec!["bash".to_string(), "-c".to_string(), script],
                docker_image: image.clone(),
                flavor: flavor.clone(),
                environment: HashMap::new(),
                secrets,
                timeout_seconds,
                labels,
            },
        )
        .await?;

        // Record the job handle: local store (the truth orx serve exposes), then
        // the api mirror (display + reconciliation). Local write must not be lost
        // even if the PATCH fails — supervise needs it to reattach.
        descriptor.job_id = Some(job.id.clone());
        descriptor.url = Some(hf::job_url(&namespace, &job.id));
        descriptor.image = Some(image);
        let store = Store::open()?;
        store.upsert_run(&StoredRun {
            id: run_id.clone(),
            experiment_id: args.exp_id.clone(),
            project_id: created.project_id.clone(),
            status: "starting".to_string(),
            backend_json: descriptor.to_json(),
            command: created.run_command.clone(),
            created_at: now_ms(),
            updated_at: now_ms(),
            ended_at: None,
            exit_code: None,
            commit_sha: None,
            result_markdown: None,
            cancel_requested: false,
        })?;
        if let Err(err) = crate::client::update_external_run(
            creds,
            &run_id,
            serde_json::json!({ "backend": serde_json::to_value(&descriptor)? }),
        )
        .await
        {
            eprintln!("warning: could not mirror the job handle to the api: {err}");
        }

        // Detach the supervisor: it tails logs, mirrors transitions, and uploads
        // the final log. Survives this process exiting (new process group).
        crate::commands::exp::spawn_detached_supervise(&run_id)?;

        println!("\u{2713} Hugging Face job submitted.");
        println!("  run    {run_id}");
        println!("  job    {}/{} ({flavor})", namespace, job.id);
        println!("  watch  {}", descriptor.url.as_deref().unwrap_or(""));
        println!(
            "  Follow it with `orx exp wait {}` or `orx logs {run_id}`.",
            args.exp_id
        );
        // Key event, fired only on success. Managed run on the user's HF account.
        crate::telemetry::capture_experiment_started("run", false, Some("hf"));
        Ok(())
    }

    /// `--backend modal` — run the experiment as a Modal Sandbox on the user's
    /// own Modal account. (Former `exp::launch_modal`.)
    async fn launch_modal(&self, args: ExpRunArgs) -> Result<()> {
        use crate::jobs::modal;
        let creds = &self.creds;
        if args.sandbox.is_some() || args.gpu.is_some() || args.cpu.is_some() {
            return Err(anyhow!(
                "--backend modal runs on Modal serverless GPUs; drop --gpu/--cpu/--sandbox \
                 and pass --flavor instead (e.g. --flavor a10g, --flavor a100-80gb, --flavor cpu)."
            ));
        }
        let flavor = args.flavor.clone().ok_or_else(|| {
            anyhow!(
                "--backend modal requires --flavor: a Modal GPU (t4, l4, a10g, a100, a100-80gb, \
                 l40s, h100, h200, or e.g. h100:2) — or cpu / cpu-large for CPU-only. \
                 Priced per second on your Modal account."
            )
        })?;
        let resources = modal::resolve_flavor(&flavor);
        // Fail before registering the run with the api if Modal isn't set up.
        modal::preflight().await?;
        let timeout_seconds = match &args.timeout {
            Some(t) => hf::parse_timeout(t)?,
            None => 4 * 3600,
        };
        const MODAL_APP: &str = "openresearch";

        // Register first: the run must exist in the tree before compute starts,
        // and the response carries the repo/branch/command orx needs to submit.
        let mut descriptor = BackendDescriptor {
            kind: "modal_job".to_string(),
            namespace: Some(MODAL_APP.to_string()),
            job_id: None,
            flavor: Some(flavor.clone()),
            image: args.image.clone(),
            url: None,
            context: None,
            manifest: None,
            resources: None,
            ssh_host: None,
            ssh_port: None,
            ssh_user: None,
            timeout_secs: None,
        };
        let created =
            create_external_run(creds, &args.exp_id, serde_json::to_value(&descriptor)?).await?;
        let run_id = created.run.id.clone();

        let image = args
            .image
            .clone()
            .unwrap_or_else(|| modal::default_image(resources.gpu.is_some()));
        let script = crate::commands::exp::hf_clone_script(
            &created.branch_name,
            &created.github_owner,
            &created.github_repo,
            &created.run_command,
        );

        // Same clone-credential precedence as the HF path: explicit GITHUB_TOKEN
        // (env, then the box's synced env file) overrides the api's repo-scoped
        // installation token.
        let mut env = HashMap::new();
        if let Ok(hf_token) = hf::resolve_token() {
            env.insert("HF_TOKEN".to_string(), hf_token);
        }
        let github_token = std::env::var("GITHUB_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty())
            .or_else(|| crate::config::synced_env_var("GITHUB_TOKEN"))
            .or_else(|| created.github_token.clone());
        if let Some(gh) = github_token {
            env.insert("GITHUB_TOKEN".to_string(), gh);
        }
        let mut tags = HashMap::new();
        tags.insert("or_run".to_string(), run_id.clone());
        tags.insert("or_experiment".to_string(), args.exp_id.clone());
        tags.insert("or_project".to_string(), created.project_id.clone());

        let sandbox_id = modal::run_job(&modal::ModalJobSpec {
            script,
            image: image.clone(),
            gpu: resources.gpu.clone(),
            cpu: resources.cpu,
            memory: resources.memory,
            env,
            timeout_seconds,
            app: MODAL_APP.to_string(),
            tags,
        })
        .await?;

        // Record the sandbox handle: local store (the truth orx serve exposes),
        // then the api mirror. The local write must not be lost even if PATCH
        // fails.
        descriptor.job_id = Some(sandbox_id.clone());
        descriptor.image = Some(image);
        let store = Store::open()?;
        store.upsert_run(&StoredRun {
            id: run_id.clone(),
            experiment_id: args.exp_id.clone(),
            project_id: created.project_id.clone(),
            status: "starting".to_string(),
            backend_json: descriptor.to_json(),
            command: created.run_command.clone(),
            created_at: now_ms(),
            updated_at: now_ms(),
            ended_at: None,
            exit_code: None,
            commit_sha: None,
            result_markdown: None,
            cancel_requested: false,
        })?;
        if let Err(err) = crate::client::update_external_run(
            creds,
            &run_id,
            serde_json::json!({ "backend": serde_json::to_value(&descriptor)? }),
        )
        .await
        {
            eprintln!("warning: could not mirror the sandbox handle to the api: {err}");
        }

        crate::commands::exp::spawn_detached_supervise(&run_id)?;

        println!("\u{2713} Modal sandbox submitted.");
        println!("  run     {run_id}");
        println!("  sandbox {sandbox_id} ({flavor})");
        println!(
            "  Follow it with `orx exp wait {}` or `orx logs {run_id}`.",
            args.exp_id
        );
        // Key event, fired only on success. Managed run on the user's Modal
        // account.
        crate::telemetry::capture_experiment_started("run", false, Some("modal"));
        Ok(())
    }

    // --- report subcommands (former commands::report server fns) ----------

    async fn report_show(&self, project_id: &str, report: &str) -> Result<()> {
        let creds = &self.creds;
        let report_id = resolve_report_id(creds, project_id, report).await?;

        let detail = get_report(creds, project_id, &report_id).await?;
        if detail.markdown.is_empty() {
            return Err(anyhow!(
                "Report {:?} has no markdown body (report.md was never uploaded).",
                detail.report.title
            ));
        }
        print!("{}", detail.markdown);
        if !detail.markdown.ends_with('\n') {
            println!();
        }
        Ok(())
    }

    async fn report_download(&self, project_id: &str, report: &str, dir: &str) -> Result<()> {
        let creds = &self.creds;
        let report_id = resolve_report_id(creds, project_id, report).await?;

        let detail = get_report(creds, project_id, &report_id).await?;
        if detail.markdown.is_empty() {
            return Err(anyhow!(
                "Report {:?} has no markdown body (report.md was never uploaded).",
                detail.report.title
            ));
        }

        let root = PathBuf::from(dir);
        std::fs::create_dir_all(&root)
            .map_err(|e| anyhow!("Could not create {}: {}", root.display(), e))?;

        // report.md, byte-for-byte (the markdown the API returns is the stored
        // file, YAML frontmatter included — the ingest reads `repo`/`gpu`/`count`
        // from it).
        let md_path = root.join("report.md");
        std::fs::write(&md_path, detail.markdown.as_bytes())
            .map_err(|e| anyhow!("Could not write {}: {}", md_path.display(), e))?;
        println!("  wrote report.md");

        // Pull every report-relative file the markdown links to (images, mostly).
        // There's no list-files endpoint, so the references in report.md are the
        // manifest — which is exactly the set that has to exist for it to render.
        let mut downloaded = 0usize;
        for rel in report_relative_links(&detail.markdown) {
            if !is_safe_report_path(&rel) {
                continue;
            }
            let bytes = match download_report_file(creds, project_id, &report_id, &rel).await {
                Ok(b) => b,
                // A broken link in the markdown shouldn't abort the whole
                // download; surface it and keep going.
                Err(e) => {
                    eprintln!("  ! skipped {} ({})", rel, e);
                    continue;
                }
            };
            let out = root.join(&rel);
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| anyhow!("Could not create {}: {}", parent.display(), e))?;
            }
            std::fs::write(&out, &bytes)
                .map_err(|e| anyhow!("Could not write {}: {}", out.display(), e))?;
            println!("  wrote {}", rel);
            downloaded += 1;
        }

        println!(
            "\u{2713} Downloaded report to {} (report.md + {} file{})",
            root.display(),
            downloaded,
            if downloaded == 1 { "" } else { "s" }
        );
        Ok(())
    }

    async fn report_list(&self, project_id: &str) -> Result<()> {
        let reports = list_reports(&self.creds, project_id).await?.reports;
        if reports.is_empty() {
            println!("No reports yet.");
            return Ok(());
        }
        for r in reports {
            println!("{}  {}  ({})", r.id, r.title, r.created_at);
        }
        Ok(())
    }

    async fn report_upload(
        &self,
        project_id: &str,
        folder: &str,
        title: Option<String>,
    ) -> Result<()> {
        let creds = &self.creds;

        let root = PathBuf::from(folder);
        if !root.is_dir() {
            return Err(anyhow!("Not a directory: {}", folder));
        }

        // Collect every file under the folder as a report-relative POSIX path.
        let mut rel_paths: Vec<String> = Vec::new();
        collect_files(&root, &root, &mut rel_paths)?;
        rel_paths.retain(|p| {
            let name = p.rsplit('/').next().unwrap_or(p);
            !IGNORED.contains(&name)
        });

        if rel_paths.is_empty() {
            return Err(anyhow!("No files found in {}", folder));
        }
        if !rel_paths.iter().any(|p| p == "report.md") {
            return Err(anyhow!(
                "{} must contain a report.md at its top level",
                folder
            ));
        }

        // Title defaults to the folder name.
        let title = title.unwrap_or_else(|| {
            root.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("report")
                .to_string()
        });

        let result = create_report(
            creds,
            project_id,
            &CreateReportBody {
                title: title.clone(),
                slug: None,
                files: rel_paths.clone(),
            },
        )
        .await?;

        // Upload each file to its presigned URL.
        for slot in &result.uploads {
            let abs = root.join(&slot.path);
            let bytes = std::fs::read(&abs)
                .map_err(|e| anyhow!("Could not read {}: {}", abs.display(), e))?;
            upload_to_presigned(&slot.url, &slot.content_type, bytes).await?;
            println!("  uploaded {}", slot.path);
        }

        println!("\u{2713} Uploaded report");
        println!("  id:    {}", result.report.id);
        println!("  title: {}", result.report.title);
        println!("  files: {}", result.uploads.len());
        Ok(())
    }
}

/// Sleep one interval, but fail with a timeout error if the deadline passed.
/// (Former `exp::sleep_until_or_timeout` — shared by both planes' wait loops.)
pub(super) async fn sleep_until_or_timeout(interval: Duration, deadline: Instant) -> Result<()> {
    if Instant::now() >= deadline {
        return Err(anyhow!("Timed out waiting for a run state change."));
    }
    let nap = interval.min(deadline.saturating_duration_since(Instant::now()));
    tokio::time::sleep(nap).await;
    if Instant::now() >= deadline {
        return Err(anyhow!("Timed out waiting for a run state change."));
    }
    Ok(())
}

// Files surfaced by the OS that aren't part of a report.
const IGNORED: &[&str] = &[".DS_Store", "Thumbs.db"];

/// Resolve a report id-or-slug to its id, erroring clearly if it isn't found.
/// We always list first so a stale ref gives a helpful message, not a raw 404.
async fn resolve_report_id(creds: &Credentials, project_id: &str, report: &str) -> Result<String> {
    let reports = list_reports(creds, project_id).await?.reports;
    reports
        .iter()
        .find(|r| r.id == report || r.slug == report)
        .map(|r| r.id.clone())
        .ok_or_else(|| {
            anyhow!(
                "No report {:?} in this project. List them with: orx report list {}",
                report,
                project_id
            )
        })
}

/// Extract the report-relative link/image targets from markdown — the `target`
/// in every `](target)` (covers `![alt](images/x.png)` and `[text](file)`).
/// Filters out absolute URLs, anchors, and absolute paths, leaving the local
/// files the report bundles. Deduplicated, order preserved.
fn report_relative_links(md: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let bytes = md.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b']' && bytes[i + 1] == b'(' {
            let start = i + 2;
            if let Some(rel) = bytes[start..].iter().position(|&b| b == b')') {
                let inner = &md[start..start + rel];
                // Drop an optional `"title"` after the URL: `(path "t")`.
                let target = inner.split_whitespace().next().unwrap_or("").trim();
                if is_local_target(target) && !out.iter().any(|p| p == target) {
                    out.push(target.to_string());
                }
                i = start + rel + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// A link target that points at a file bundled in the report (not the web).
fn is_local_target(t: &str) -> bool {
    !t.is_empty()
        && !t.starts_with('#')
        && !t.starts_with('/')
        && !t.contains("://")
        && !t.starts_with("//")
        && !t.starts_with("mailto:")
        && !t.starts_with("data:")
}

/// Mirror of the server's `isSafeReportPath`: relative, no `..`/`.` segments,
/// no backslashes — so a malicious markdown link can't escape `dir`.
fn is_safe_report_path(p: &str) -> bool {
    !p.starts_with('/') && !p.contains('\\') && !p.split('/').any(|seg| seg == ".." || seg == ".")
}

/// Recursively collect files under `dir`, pushing each as a `/`-joined path
/// relative to `base`.
fn collect_files(base: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    for entry in
        std::fs::read_dir(dir).map_err(|e| anyhow!("Could not read {}: {}", dir.display(), e))?
    {
        let entry = entry.map_err(|e| anyhow!("Could not read entry: {}", e))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|e| anyhow!("Could not stat {}: {}", path.display(), e))?;
        if file_type.is_dir() {
            collect_files(base, &path, out)?;
        } else if file_type.is_file() {
            if let Ok(rel) = path.strip_prefix(base) {
                let rel = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                if !rel.is_empty() {
                    out.push(rel);
                }
            }
        }
    }
    Ok(())
}
