//! Local HF Jobs launch — mirrors `commands/exp.rs::launch_hf` with the api
//! calls deleted: the run row comes from and goes to the local store only.
//! The detached `orx supervise` it spawns detects local runs itself.

use std::collections::HashMap;

use crate::commands::exp::{default_hf_image, hf_clone_script, spawn_detached_supervise};
use crate::error::{anyhow, Result};
use crate::jobs::{huggingface as hf, BackendDescriptor};
use crate::local::git;
use crate::store::{now_ms, Store, StoredRun};

/// CLI wrapper around `submit_local_hf`: submit, then print the summary.
pub async fn launch_local_hf(args: &crate::ExpRunArgs) -> Result<()> {
    let run = submit_local_hf(args).await?;
    let backend = crate::jobs::BackendDescriptor::parse(&run.backend_json)?;
    println!("\u{2713} Hugging Face job submitted.");
    println!("  run    {}", run.id);
    println!(
        "  job    {}/{} ({})",
        backend.namespace.as_deref().unwrap_or(""),
        backend.job_id.as_deref().unwrap_or(""),
        backend.flavor.as_deref().unwrap_or("")
    );
    println!("  watch  {}", backend.url.as_deref().unwrap_or(""));
    println!(
        "  Follow it with `orx exp wait {}` or `orx logs {}`.",
        run.experiment_id, run.id
    );
    Ok(())
}

/// Submit the local experiment's run as a Hugging Face Job and detach a
/// supervisor. `args.exp_id` must exist in `local_experiments`; requires
/// `--backend hf` and `--flavor`. Shared by the CLI and the `orx up` API.
pub async fn submit_local_hf(args: &crate::ExpRunArgs) -> Result<StoredRun> {
    if args.sandbox.is_some() || args.gpu.is_some() || args.cpu.is_some() {
        return Err(anyhow!(
            "Local experiments run on Hugging Face Jobs; drop --gpu/--cpu/--sandbox \
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
    // Same default as the server path — HF's own 30m default is a footgun.
    let timeout_seconds = match &args.timeout {
        Some(t) => hf::parse_timeout(t)?,
        None => 4 * 3600,
    };

    let store = Store::open()?;
    let exp = store
        .get_local_experiment(&args.exp_id)?
        .ok_or_else(|| anyhow!("Local experiment {} not found.", args.exp_id))?;
    let project = store
        .get_local_project(&exp.project_id)?
        .ok_or_else(|| anyhow!("Local project {} not found.", exp.project_id))?;
    if let Some(w) = crate::local::experiments::legacy_root_warning(&project, &exp) {
        eprintln!("{w}");
    }
    // Experiment command, else the project default.
    let run_command = Some(exp.run_command.clone())
        .filter(|c| !c.trim().is_empty())
        .or_else(|| project.run_command.clone().filter(|c| !c.trim().is_empty()))
        .ok_or_else(|| {
            anyhow!(
                "No run command set for this experiment or its project. Set the project \
                 default with `orx project edit {} --run-command '<cmd>'`, pass \
                 `--run-command '<cmd>'` to `orx create-experiment`, or set it in the \
                 dashboard — then relaunch.",
                project.id
            )
        })?;

    // One run in flight per experiment unless the caller deliberately forces
    // a concurrent launch — the double-click / double-submit guard.
    if !args.force {
        if let Some(r) = store
            .list_runs_by_experiment(&exp.id)?
            .into_iter()
            .find(|r| !crate::local::is_terminal(&r.status))
        {
            return Err(anyhow!(
                "Run {} is already in flight for this experiment ({}). \
                 Cancel it with `orx exp cancel {}` or pass --force to launch anyway.",
                r.id,
                r.status,
                exp.id
            ));
        }
    }

    let token = hf::resolve_token()?;
    let namespace = hf::whoami(&token).await?;

    // The job clones from GitHub, so the branch tip must exist there. Fetch
    // via ensure_clone so branch_head_sha matches what the job will check out.
    // Git shells out (network, can stall) — keep it off the async workers.
    let commit_sha = {
        let (owner, repo, baseline, branch) = (
            project.github_owner.clone(),
            project.github_repo.clone(),
            project.baseline_branch.clone(),
            exp.branch_name.clone(),
        );
        tokio::task::spawn_blocking(move || -> Result<String> {
            let repo_path = git::ensure_clone(&owner, &repo, &baseline)?;
            if !git::branch_on_remote(&repo_path, &branch)? {
                git::push_branch(&repo_path, &branch)?;
            }
            git::branch_head_sha(&repo_path, &branch)
        })
        .await
        .map_err(|e| anyhow!("git task failed: {e}"))??
    };

    let run_id = uuid::Uuid::new_v4().to_string();
    let image = args
        .image
        .clone()
        .unwrap_or_else(|| default_hf_image(&flavor));
    let script = hf_clone_script(
        &exp.branch_name,
        &project.github_owner,
        &project.github_repo,
        &run_command,
    );

    // Tokens travel as job secrets only — the command line stays tokenless.
    let mut secrets = HashMap::new();
    secrets.insert("HF_TOKEN".to_string(), token.clone());
    if let Some(gh) = git::resolve_github_token() {
        secrets.insert("GITHUB_TOKEN".to_string(), gh);
    }
    let mut labels = HashMap::new();
    labels.insert("or_run".to_string(), run_id.clone());
    labels.insert("or_experiment".to_string(), exp.id.clone());
    labels.insert("or_project".to_string(), project.id.clone());

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

    let descriptor = BackendDescriptor {
        kind: "hf_job".to_string(),
        namespace: Some(namespace.clone()),
        job_id: Some(job.id.clone()),
        flavor: Some(flavor.clone()),
        image: Some(image),
        url: Some(hf::job_url(&namespace, &job.id)),
        context: None,
        manifest: None,
        resources: None,
        ssh_host: None,
        ssh_port: None,
        ssh_user: None,
        timeout_secs: None,
    };
    let run = StoredRun {
        id: run_id.clone(),
        experiment_id: exp.id.clone(),
        project_id: project.id.clone(),
        status: "starting".to_string(),
        backend_json: descriptor.to_json(),
        command: run_command,
        created_at: now_ms(),
        updated_at: now_ms(),
        ended_at: None,
        exit_code: None,
        commit_sha: Some(commit_sha),
        result_markdown: None,
        cancel_requested: false,
    };
    store.upsert_run(&run)?;

    spawn_detached_supervise(&run_id)?;
    Ok(run)
}
