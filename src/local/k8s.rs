//! Local Kubernetes launch — the k8s twin of `local/hf.rs`: the run row comes
//! from and goes to the local store only, and the detached `orx supervise`
//! watches the Job via kubectl. Cluster/namespace/flavors come from the
//! settings file (`orx up` Settings → Compute, or `~/.config/openresearch/k8s.json`).

use std::collections::HashMap;

use crate::commands::exp::{hf_clone_script, spawn_detached_supervise};
use crate::error::{anyhow, Result};
use crate::jobs::{huggingface as hf, kubernetes as k8s, BackendDescriptor};
use crate::local::git;
use crate::store::{now_ms, Store, StoredRun};

/// CLI wrapper around `submit_local_k8s`: submit, then print the summary.
pub async fn launch_local_k8s(args: &crate::ExpRunArgs) -> Result<()> {
    let run = submit_local_k8s(args).await?;
    let backend = BackendDescriptor::parse(&run.backend_json)?;
    println!("\u{2713} Kubernetes job submitted.");
    println!(
        "  job    {}/{} ({})",
        backend.namespace.as_deref().unwrap_or(""),
        backend.job_id.as_deref().unwrap_or(""),
        backend.flavor.as_deref().unwrap_or("")
    );
    println!("  run    {}", run.id);
    println!(
        "  Follow it with `orx exp wait {}` or `orx logs {}`.",
        run.experiment_id, run.id
    );
    Ok(())
}

/// Submit the local experiment's run as a Kubernetes Job and detach a
/// supervisor. Requires `--backend k8s` and `--flavor <name>` where the name
/// is a detected or custom flavor from the k8s settings.
pub async fn submit_local_k8s(args: &crate::ExpRunArgs) -> Result<StoredRun> {
    if args.sandbox.is_some() || args.gpu.is_some() || args.cpu.is_some() {
        return Err(anyhow!(
            "--backend k8s runs on your Kubernetes cluster; drop --gpu/--cpu/--sandbox \
             and pass --flavor instead (see `orx up` Settings → Compute for names)."
        ));
    }
    let settings = k8s::load_settings()?.unwrap_or_default();
    let flavors = settings.all_flavors();
    let flavor_name = args.flavor.clone().ok_or_else(|| flavor_error(&flavors))?;
    let flavor = settings
        .resolve_flavor(&flavor_name)
        .ok_or_else(|| flavor_error(&flavors))?;
    // Same default as the HF path — no-timeout jobs are a footgun.
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

    // One run in flight per experiment unless deliberately forced.
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

    // The job clones from GitHub, so the branch tip must exist there.
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
        .or_else(|| settings.default_image.clone())
        .unwrap_or_else(|| {
            if flavor.gpu > 0 {
                "pytorch/pytorch:2.6.0-cuda12.4-cudnn9-runtime".to_string()
            } else {
                "python:3.12".to_string()
            }
        });
    let script = hf_clone_script(
        &exp.branch_name,
        &project.github_owner,
        &project.github_repo,
        &run_command,
    );

    // The pod's env: everything the user synced (API keys), plus the tokens
    // the clone script and common tooling expect. Travels via a k8s Secret,
    // never on a command line.
    let mut env: HashMap<String, String> = crate::config::list_synced_env().into_iter().collect();
    if let Ok(hf_token) = hf::resolve_token() {
        env.entry("HF_TOKEN".to_string()).or_insert(hf_token);
    }
    if let Some(gh) = git::resolve_github_token() {
        env.insert("GITHUB_TOKEN".to_string(), gh);
    }
    let mut labels = HashMap::new();
    labels.insert("or_run".to_string(), run_id.clone());
    labels.insert("or_experiment".to_string(), exp.id.clone());
    labels.insert("or_project".to_string(), project.id.clone());

    let context = settings.context.clone();
    let namespace = settings.namespace.clone();
    let job_name = k8s::run_job(
        context.as_deref(),
        &namespace,
        &k8s::K8sJobSpec {
            script,
            image: image.clone(),
            flavor: flavor.clone(),
            env,
            timeout_seconds,
            labels,
        },
    )
    .await?;

    let descriptor = BackendDescriptor {
        kind: "k8s_job".to_string(),
        namespace: Some(namespace),
        job_id: Some(job_name),
        flavor: Some(flavor.name.clone()),
        image: Some(image),
        url: None,
        context,
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

fn flavor_error(flavors: &[k8s::Flavor]) -> crate::error::Error {
    if flavors.is_empty() {
        anyhow!(
            "--backend k8s requires --flavor, and no flavors are configured yet. \
             Open `orx up` Settings → Compute to pick a cluster and auto-detect flavors."
        )
    } else {
        let names: Vec<&str> = flavors.iter().map(|f| f.name.as_str()).collect();
        anyhow!(
            "--backend k8s requires --flavor: {}. Manage them in `orx up` Settings → Compute.",
            names.join(", ")
        )
    }
}
