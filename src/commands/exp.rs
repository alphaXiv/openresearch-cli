//! The `exp` command group: operate on a single experiment node by id.
//!
//!   orx exp status <expId>            inspect status, run command, latest run
//!   orx exp cmd    <expId> [--set …]  view or set the run command
//!   orx exp run    <expId> …          launch a run on new or existing compute
//!   orx exp cancel <expId>            cancel the in-flight run
//!
//! Unlike the project-scoped data commands, every verb here takes an
//! *experiment* id (from `orx experiments <projectId>`).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;

use crate::client::{
    cancel_experiment_run, find_project, get_experiment, list_runs, start_experiment_run,
    update_experiment, RunTarget, UpdateExperimentBody,
};
use crate::error::{anyhow, require_credentials, Result};
use crate::{ExpCommand, ExpRunArgs};

pub async fn run(args: crate::ExpArgs) -> Result<()> {
    let creds = require_credentials().await;
    match args.command {
        ExpCommand::Status { exp_id } => status(&creds, &exp_id).await,
        ExpCommand::Cmd { exp_id, set } => cmd(&creds, &exp_id, set).await,
        ExpCommand::Desc { exp_id, set, stdin } => desc(&creds, &exp_id, set, stdin).await,
        ExpCommand::Run(run_args) => launch(&creds, run_args).await,
        ExpCommand::Cancel { exp_id } => cancel(&creds, &exp_id).await,
        ExpCommand::Wait {
            exp_id,
            project,
            timeout,
            interval,
        } => wait(&creds, exp_id, project, timeout, interval).await,
    }
}

/// Terminal run states — the run is finished and won't change further.
fn is_terminal(status: &str) -> bool {
    matches!(status, "done" | "failed" | "cancelled")
}

/// `orx exp wait …` — block on run state, for agents driving a research loop.
///
/// Two modes, picked by argument:
///   - `<expId>` — level trigger: poll the experiment's latest run until it reaches a terminal state (done/failed/cancelled).
///   - `--project` — edge trigger: snapshot every run in the project and return when the first run *completes* — i.e. transitions into a terminal state (done/failed/cancelled). This is the "a slot just freed" signal a budget-saturation loop wants; run starts and queued→running transitions are intentionally ignored.
///
/// Polls every `--interval` seconds (default 5), gives up after `--timeout`
/// seconds (default 1800) with a non-zero exit so callers can branch on it.
async fn wait(
    creds: &crate::config::Credentials,
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
        (Some(exp_id), None) => wait_experiment(creds, &exp_id, interval, deadline).await,
        (None, Some(project_id)) => wait_project(creds, &project_id, interval, deadline).await,
    }
}

/// Level trigger: poll one experiment's latest run until it's terminal.
async fn wait_experiment(
    creds: &crate::config::Credentials,
    exp_id: &str,
    interval: Duration,
    deadline: Instant,
) -> Result<()> {
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
                if is_terminal(&r.status) {
                    println!("{} {}", r.id, r.status);
                    return Ok(());
                }
            }
        }
        sleep_until_or_timeout(interval, deadline).await?;
    }
}

/// Edge trigger: return as soon as any run in the project *completes* — i.e.
/// transitions into a terminal state (done/failed/cancelled) vs. the snapshot
/// taken on entry. Run starts, new queued runs, and queued→running transitions
/// are ignored: the useful project-wide signal is "a slot just freed", so a
/// budget-saturation loop can analyze the finished run and launch the next one.
///
/// Note: runs already terminal at entry don't count (they're in the snapshot as
/// terminal). If *every* run is already terminal when this is called, there's
/// nothing left to complete — it returns immediately printing
/// `drained: no runs in flight` (exit 0), the termination signal for a budget
/// loop. Otherwise it returns on the first completion.
///
/// This fires only on completions observed *within a single invocation*. A run
/// that finishes between two calls (while the caller is deciding/analyzing) is
/// already terminal in the next call's entry snapshot and won't fire. So the
/// caller must treat `exp wait --project` as a sleep-until-change signal and
/// re-list `orx runs` on every wake to find *all* newly-finished runs — don't
/// trust the printed line as the complete set.
async fn wait_project(
    creds: &crate::config::Credentials,
    project_id: &str,
    interval: Duration,
    deadline: Instant,
) -> Result<()> {
    let snapshot: HashMap<String, String> = list_runs(creds, project_id)
        .await?
        .runs
        .into_iter()
        .map(|r| (r.id, r.status))
        .collect();
    let in_flight = snapshot.values().filter(|s| !is_terminal(s)).count();

    // Fast path: nothing is in flight, so no run can *complete* while we watch —
    // there's nothing to wait for. Rather than block until `--timeout` (the old
    // behavior), return immediately with a distinct, machine-readable line so a
    // budget loop can recognize "the batch is drained" and stop looping. This is
    // the clean termination signal for `orx exp wait --project` in a loop.
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
        let mut completed: Vec<String> = Vec::new();
        for r in &current {
            if !is_terminal(&r.status) {
                continue;
            }
            // Fire only on a *new* terminal: a run that was non-terminal before,
            // or a brand-new run that's already terminal. Skip runs that were
            // already terminal in the entry snapshot.
            match snapshot.get(&r.id) {
                Some(prev) if is_terminal(prev) => {}
                Some(prev) => completed.push(format!("{} {} -> {}", r.id, prev, r.status)),
                None => completed.push(format!("{} {} (new)", r.id, r.status)),
            }
        }
        if !completed.is_empty() {
            for c in &completed {
                println!("{}", c);
            }
            return Ok(());
        }
    }
}

/// Sleep one interval, but fail with a timeout error if the deadline passed.
async fn sleep_until_or_timeout(interval: Duration, deadline: Instant) -> Result<()> {
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

/// `orx exp status <expId>` — the experiment row joined with its latest run,
/// plus everything needed to diff the run locally: the node's branch, the
/// parent's branch, the full commit SHA, and a ready-to-paste git recipe.
async fn status(creds: &crate::config::Credentials, exp_id: &str) -> Result<()> {
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

    println!("{}  ({})", exp.title, exp.status);
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
            let commit = r
                .commit_sha
                .as_ref()
                .map(|s| s.chars().take(7).collect::<String>())
                .unwrap_or_else(|| "—".to_string());
            println!(
                "  last run: {} ({}, commit {}, updated {})",
                r.id, r.status, commit, r.updated_at
            );
            if let Some(sha) = r.commit_sha {
                println!("  commit:   {}", sha);
                full_sha = Some(sha);
            }
        }
        None => println!("  last run: — (never run)"),
    }

    // Local diff recipe — only when there's both a base (parent branch) and a
    // head (run commit) to compare. Owner/repo lookup is best-effort too: on
    // failure print placeholders the caller can fill from `orx projects`.
    if let (Some(branch), Some(sha)) = (parent_branch, full_sha) {
        let repo_path = match find_project(creds, &exp.project_id).await {
            Ok(Some(p)) if !p.github_owner.is_empty() && !p.github_repo.is_empty() => {
                format!("{}/{}", p.github_owner, p.github_repo)
            }
            _ => "<owner>/<repo>".to_string(),
        };
        let dir = format!("~/.cache/openresearch/repos/{}", repo_path);
        println!();
        println!("To see what this run changed vs. its parent, in your local clone:");
        if repo_path == "<owner>/<repo>" {
            println!("  # owner/repo from `orx projects`");
        }
        println!("  git -C {} fetch origin", dir);
        println!("  git -C {} diff origin/{}...{}", dir, branch, sha);
    }

    Ok(())
}

/// `orx exp cmd <expId> [--set <command>]` — view or set the run command.
async fn cmd(creds: &crate::config::Credentials, exp_id: &str, set: Option<String>) -> Result<()> {
    match set {
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

/// `orx exp desc <expId> [--set <text> | --stdin]` — view or overwrite the
/// experiment's free-form description / notes (the existing `description` field).
async fn desc(
    creds: &crate::config::Credentials,
    exp_id: &str,
    set: Option<String>,
    stdin: bool,
) -> Result<()> {
    // Resolve the new value, if this is a write. `--set` and `--stdin` are
    // mutually exclusive; either present means "overwrite".
    let new_desc = match (set, stdin) {
        (Some(_), true) => return Err(anyhow!("Pass either --set or --stdin, not both.")),
        (Some(text), false) => Some(text),
        (None, true) => {
            let mut buf = String::new();
            tokio::io::stdin().read_to_string(&mut buf).await?;
            Some(buf)
        }
        (None, false) => None,
    };

    match new_desc {
        // Write path: overwrite the whole description.
        Some(description) => {
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
        None => {
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

/// `orx exp run <expId> …` — launch a run on a new instance or existing sandbox.
async fn launch(creds: &crate::config::Credentials, args: ExpRunArgs) -> Result<()> {
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
    let target = if let Some(sandbox_id) = &args.sandbox {
        RunTarget::Existing {
            sandbox_id: sandbox_id.clone(),
        }
    } else if let Some(gpu) = &args.gpu {
        RunTarget::New {
            gpu: gpu.clone(),
            gpu_count: args.count.unwrap_or(1),
            disk_gb: args.disk.unwrap_or(100),
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

    // Friendlier than the raw API "No run command set": tell them how to fix it.
    let current = get_experiment(creds, &args.exp_id).await?;
    if current.experiment.run_command.is_empty() {
        return Err(anyhow!(
            "No run command set for this experiment. Set one first with \
             `orx exp cmd {} --set \"…\"`.",
            args.exp_id
        ));
    }

    start_experiment_run(creds, &args.exp_id, target, args.force).await?;

    println!("\u{2713} Run queued.");
    println!(
        "  Follow it with `orx runs {}` and `orx logs <runId>`.",
        current.experiment.project_id
    );
    Ok(())
}

/// `orx exp cancel <expId>` — terminate the in-flight run, if any.
async fn cancel(creds: &crate::config::Credentials, exp_id: &str) -> Result<()> {
    cancel_experiment_run(creds, exp_id).await?;
    println!("\u{2713} Run cancelled.");
    Ok(())
}
