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
    cancel_experiment_run, get_experiment, list_runs, start_experiment_run, update_experiment,
    RunTarget, UpdateExperimentBody,
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
///   - `--project` — edge trigger: snapshot every run in the project and return on the first change of any kind (new run or status change).
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

/// Edge trigger: return as soon as any run in the project changes vs. the
/// snapshot taken on entry (new run id, or a status transition).
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
    eprintln!(
        "Watching {} run(s) in project — returning on the first change…",
        snapshot.len()
    );

    loop {
        sleep_until_or_timeout(interval, deadline).await?;

        let current = list_runs(creds, project_id).await?.runs;
        let mut changes: Vec<String> = Vec::new();
        for r in &current {
            match snapshot.get(&r.id) {
                None => changes.push(format!("{} {} (new)", r.id, r.status)),
                Some(prev) if prev != &r.status => {
                    changes.push(format!("{} {} -> {}", r.id, prev, r.status))
                }
                Some(_) => {}
            }
        }
        if !changes.is_empty() {
            for c in &changes {
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

/// `orx exp status <expId>` — the experiment row joined with its latest run.
async fn status(creds: &crate::config::Credentials, exp_id: &str) -> Result<()> {
    let res = get_experiment(creds, exp_id).await?;
    let exp = res.experiment;

    println!("{}  ({})", exp.title, exp.status);
    println!("  id:       {}", exp.id);
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
        }
        None => println!("  last run: — (never run)"),
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
    // Resolve the target: exactly one of --sandbox or --gpu.
    let target = match (&args.sandbox, &args.gpu) {
        (Some(_), Some(_)) => {
            return Err(anyhow!("Pass either --sandbox or --gpu, not both."));
        }
        (None, None) => {
            return Err(anyhow!(
                "Choose compute: --gpu <id> [--count N] [--disk GB], or --sandbox <id>. \
                 See `orx compute` for available GPUs."
            ));
        }
        (Some(sandbox_id), None) => RunTarget::Existing {
            sandbox_id: sandbox_id.clone(),
        },
        (None, Some(gpu)) => RunTarget::New {
            gpu: gpu.clone(),
            gpu_count: args.count.unwrap_or(1),
            disk_gb: args.disk.unwrap_or(100),
        },
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

    start_experiment_run(creds, &args.exp_id, target).await?;

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
