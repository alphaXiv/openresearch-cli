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
    cancel_experiment_run, create_external_run, find_project, get_experiment, list_runs,
    start_experiment_run, update_experiment, RunTarget, UpdateExperimentBody,
};
use crate::error::{anyhow, require_credentials, Result};
use crate::jobs::{huggingface as hf, BackendDescriptor};
use crate::local::model::LocalExperiment;
use crate::output::{format_duration, run_failure_detail};
use crate::store::{now_ms, Store, StoredRun};
use crate::{ExpCommand, ExpRunArgs};

pub async fn run(args: crate::ExpArgs) -> Result<()> {
    // Local-mode detection first: an id in `local_experiments` takes the local
    // path, and credentials are only required on the server path (a local-only
    // user may never have logged in).
    let store = Store::open()?;
    match args.command {
        ExpCommand::Status { exp_id } => {
            if let Some(exp) = store.get_local_experiment(&exp_id)? {
                return local_status(&store, &exp);
            }
            let creds = require_credentials().await;
            status(&creds, &exp_id).await
        }
        ExpCommand::Cmd { exp_id, set } => {
            if store.get_local_experiment(&exp_id)?.is_some() {
                return Err(crate::local::unsupported("exp cmd"));
            }
            let creds = require_credentials().await;
            cmd(&creds, &exp_id, set).await
        }
        ExpCommand::Desc { exp_id, set, stdin } => {
            if let Some(exp) = store.get_local_experiment(&exp_id)? {
                return local_desc(&store, exp, set, stdin).await;
            }
            let creds = require_credentials().await;
            desc(&creds, &exp_id, set, stdin).await
        }
        ExpCommand::Run(run_args) => {
            if store.get_local_experiment(&run_args.exp_id)?.is_some() {
                return local_launch(run_args).await;
            }
            let creds = require_credentials().await;
            launch(&creds, run_args).await
        }
        ExpCommand::Cancel { exp_id } => {
            if let Some(exp) = store.get_local_experiment(&exp_id)? {
                return local_cancel(&store, &exp);
            }
            let creds = require_credentials().await;
            cancel(&creds, &exp_id).await
        }
        ExpCommand::Wait {
            exp_id,
            project,
            timeout,
            interval,
        } => wait(&store, exp_id, project, timeout, interval).await,
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
    store: &Store,
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
        (Some(exp_id), None) => {
            if store.get_local_experiment(&exp_id)?.is_some() {
                return local_wait_experiment(store, &exp_id, interval, deadline).await;
            }
            let creds = require_credentials().await;
            wait_experiment(&creds, &exp_id, interval, deadline).await
        }
        (None, Some(project_id)) => {
            if store.get_local_project(&project_id)?.is_some() {
                return local_wait_project(store, &project_id, interval, deadline).await;
            }
            let creds = require_credentials().await;
            wait_project(&creds, &project_id, interval, deadline).await
        }
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
                    if let Some(detail) = run_failure_detail(&r) {
                        eprintln!("{detail}");
                    }
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
        // Each entry pairs the transition line with the run's failure detail (if
        // it failed) so we can surface *why* on stderr after the machine line.
        let mut completed: Vec<(String, Option<String>)> = Vec::new();
        for r in &current {
            if !is_terminal(&r.status) {
                continue;
            }
            // Fire only on a *new* terminal: a run that was non-terminal before,
            // or a brand-new run that's already terminal. Skip runs that were
            // already terminal in the entry snapshot.
            let line = match snapshot.get(&r.id) {
                Some(prev) if is_terminal(prev) => continue,
                Some(prev) => format!("{} {} -> {}", r.id, prev, r.status),
                None => format!("{} {} (new)", r.id, r.status),
            };
            completed.push((line, run_failure_detail(r)));
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
            let commit = r
                .commit_sha
                .as_ref()
                .map(|s| s.chars().take(7).collect::<String>())
                .unwrap_or_else(|| "—".to_string());
            println!(
                "  last run: {} ({}, commit {}, ran {}, updated {})",
                r.id,
                r.status,
                commit,
                format_duration(r.duration_seconds),
                r.updated_at
            );
            if let Some(detail) = run_failure_detail(&r) {
                println!("  {detail}");
            }
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
/// Resolve the new description for `exp desc`, if this is a write. `--set`
/// and `--stdin` are mutually exclusive; either present means "overwrite".
async fn resolve_desc_input(set: Option<String>, stdin: bool) -> Result<Option<String>> {
    match (set, stdin) {
        (Some(_), true) => Err(anyhow!("Pass either --set or --stdin, not both.")),
        (Some(text), false) => Ok(Some(text)),
        (None, true) => {
            let mut buf = String::new();
            tokio::io::stdin().read_to_string(&mut buf).await?;
            Ok(Some(buf))
        }
        (None, false) => Ok(None),
    }
}

async fn desc(
    creds: &crate::config::Credentials,
    exp_id: &str,
    set: Option<String>,
    stdin: bool,
) -> Result<()> {
    let new_desc = resolve_desc_input(set, stdin).await?;

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
    // External backends: orx submits and supervises the job itself; the api
    // only mirrors. Everything below this branch is the managed path.
    match args.backend.as_deref() {
        Some("hf") => return launch_hf(creds, args).await,
        Some(other) => {
            return Err(anyhow!(
                "Unknown --backend '{}'. Supported: hf (Hugging Face Jobs).",
                other
            ));
        }
        None => {}
    }
    if args.flavor.is_some() || args.image.is_some() || args.timeout.is_some() {
        return Err(anyhow!(
            "--flavor/--image/--timeout only apply with --backend hf."
        ));
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
            // Omitted = server default (RunPod). The server validates the name
            // and 400s on an unknown provider, so no client-side check.
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

/// `orx exp run <expId> --backend hf --flavor <flavor>` — run the experiment
/// as a Hugging Face Job on the user's own HF account.
///
/// Flow: register the mirror run with the api (which returns repo/branch/
/// command), submit the job natively (clone the branch tip, run the command),
/// record the job handle everywhere, then detach `orx supervise <runId>` to
/// tail logs and mirror status. Returns immediately, like the managed path.
async fn launch_hf(creds: &crate::config::Credentials, args: ExpRunArgs) -> Result<()> {
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
    // HF's own default is 30 minutes — a footgun for training runs, so default
    // generously and let --timeout tighten it.
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
    };
    let created =
        create_external_run(creds, &args.exp_id, serde_json::to_value(&descriptor)?).await?;
    let run_id = created.run.id.clone();

    let image = args
        .image
        .clone()
        .unwrap_or_else(|| default_hf_image(&flavor));
    let script = hf_clone_script(
        &created.branch_name,
        &created.github_owner,
        &created.github_repo,
        &created.run_command,
    );

    let mut secrets = HashMap::new();
    secrets.insert("HF_TOKEN".to_string(), token.clone());
    // Clone credential precedence: explicit GITHUB_TOKEN (env, then the box's
    // synced env file) overrides; otherwise the api's repo-scoped installation
    // token flows automatically from the org's connected GitHub app — a
    // private repo needs zero extra setup beyond having connected it.
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
    spawn_detached_supervise(&run_id)?;

    println!("\u{2713} Hugging Face job submitted.");
    println!("  run    {run_id}");
    println!("  job    {}/{} ({flavor})", namespace, job.id);
    println!("  watch  {}", descriptor.url.as_deref().unwrap_or(""));
    println!(
        "  Follow it with `orx exp wait {}` or `orx logs {run_id}`.",
        args.exp_id
    );
    Ok(())
}

/// Clone the experiment branch tip and run the fixed command. GITHUB_TOKEN
/// (passed as a job secret when present locally) authenticates private
/// repos via bash's ${VAR:+...} — the URL stays tokenless without it.
pub(crate) fn hf_clone_script(branch: &str, owner: &str, repo: &str, cmd: &str) -> String {
    format!(
        "set -eo pipefail; command -v git >/dev/null 2>&1 || (apt-get update -qq && apt-get install -y -qq git); \
         git clone --depth 1 --branch {branch} \"https://${{GITHUB_TOKEN:+x-access-token:${{GITHUB_TOKEN}}@}}github.com/{owner}/{repo}.git\" repo; \
         cd repo; {cmd}"
    )
}

/// Default docker image per flavor family: plain python for CPU flavors, a
/// CUDA-ready pytorch image for GPU flavors. Override with --image.
pub(crate) fn default_hf_image(flavor: &str) -> String {
    if flavor.starts_with("cpu") {
        "python:3.12".to_string()
    } else {
        "pytorch/pytorch:2.6.0-cuda12.4-cudnn9-runtime".to_string()
    }
}

/// Spawn `orx supervise <runId>` fully detached (own process group, no stdio),
/// so it outlives this command and any SSH session that launched it.
pub(crate) fn spawn_detached_supervise(run_id: &str) -> Result<()> {
    let exe = std::env::current_exe().map_err(|e| {
        anyhow!(
            "Could not locate the orx binary to spawn the supervisor: {}",
            e
        )
    })?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("supervise")
        .arg(run_id)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()
        .map_err(|e| anyhow!("Could not spawn `orx supervise {}`: {}", run_id, e))?;
    Ok(())
}

/// `orx exp cancel <expId>` — terminate the in-flight run, if any.
async fn cancel(creds: &crate::config::Credentials, exp_id: &str) -> Result<()> {
    cancel_experiment_run(creds, exp_id).await?;
    println!("\u{2713} Run cancelled.");
    Ok(())
}

// --- local mode (orx up) ---

/// Local `exp status`: the store row plus its latest run.
fn local_status(store: &Store, exp: &LocalExperiment) -> Result<()> {
    println!("{}  ({})  [local]", exp.display_name(), exp.agent_status);
    println!("  id:       {}", exp.id);
    println!("  branch:   {}", exp.branch_name);
    match &exp.parent_experiment_id {
        Some(parent_id) => match store.get_local_experiment(parent_id)? {
            Some(parent) => println!("  parent:   {} (branch {})", parent_id, parent.branch_name),
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
            let commit = r
                .commit_sha
                .as_deref()
                .map(|s| s.chars().take(7).collect::<String>())
                .unwrap_or_else(|| "—".to_string());
            println!(
                "  last run: {} ({}, commit {}, ran {}, updated {})",
                r.id,
                r.status,
                commit,
                format_duration(crate::local::run_duration_secs(&r)),
                crate::local::fmt_ago(r.updated_at)
            );
            if let Some(detail) = crate::local::run_failure_detail(&r) {
                println!("  {detail}");
            }
            if let Some(sha) = &r.commit_sha {
                println!("  commit:   {}", sha);
            }
        }
        None => println!("  last run: — (never run)"),
    }
    Ok(())
}

/// Local `exp desc`: read/write the description on the store row.
async fn local_desc(
    store: &Store,
    mut exp: LocalExperiment,
    set: Option<String>,
    stdin: bool,
) -> Result<()> {
    match resolve_desc_input(set, stdin).await? {
        Some(description) => {
            exp.description = Some(description);
            store.update_local_experiment(&exp)?;
            println!("\u{2713} Description saved.");
        }
        None => match exp.description.as_deref().filter(|d| !d.trim().is_empty()) {
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

/// Local `exp run`: external backends only — HF Jobs for v1.
async fn local_launch(args: ExpRunArgs) -> Result<()> {
    match args.backend.as_deref() {
        Some("hf") => crate::local::hf::launch_local_hf(&args).await,
        Some(other) => Err(anyhow!(
            "Unknown --backend '{}'. Local experiments support: hf (Hugging Face Jobs).",
            other
        )),
        None => Err(anyhow!(
            "Local experiments run on external compute only. \
             Pass `--backend hf --flavor <flavor>` (e.g. --flavor a10g-small)."
        )),
    }
}

/// Local `exp cancel`: flag cancel intent on every in-flight run (concurrent
/// runs are possible via --force); the local supervisors cancel the HF jobs.
fn local_cancel(store: &Store, exp: &LocalExperiment) -> Result<()> {
    let in_flight: Vec<_> = store
        .list_runs_by_experiment(&exp.id)?
        .into_iter()
        .filter(|r| !is_terminal(&r.status))
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

/// Local twin of `wait_experiment`: poll the store's latest run until terminal.
async fn local_wait_experiment(
    store: &Store,
    exp_id: &str,
    interval: Duration,
    deadline: Instant,
) -> Result<()> {
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
                if is_terminal(&r.status) {
                    println!("{} {}", r.id, r.status);
                    if let Some(detail) = crate::local::run_failure_detail(&r) {
                        eprintln!("{detail}");
                    }
                    return Ok(());
                }
            }
        }
        sleep_until_or_timeout(interval, deadline).await?;
    }
}

/// Local twin of `wait_project`: same edge-trigger semantics over the store.
async fn local_wait_project(
    store: &Store,
    project_id: &str,
    interval: Duration,
    deadline: Instant,
) -> Result<()> {
    let snapshot: HashMap<String, String> = store
        .list_runs_by_project(project_id)?
        .into_iter()
        .map(|r| (r.id, r.status))
        .collect();
    let in_flight = snapshot.values().filter(|s| !is_terminal(s)).count();

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
            if !is_terminal(&r.status) {
                continue;
            }
            let line = match snapshot.get(&r.id) {
                Some(prev) if is_terminal(prev) => continue,
                Some(prev) => format!("{} {} -> {}", r.id, prev, r.status),
                None => format!("{} {} (new)", r.id, r.status),
            };
            completed.push((line, crate::local::run_failure_detail(r)));
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
