//! Local Modal launch — the Modal twin of `local/hf.rs` and `local/k8s.rs`: the
//! run row comes from and goes to the local store only, and the detached
//! `orx supervise` watches the sandbox via the Modal Python launcher. Auth is
//! Modal's own (MODAL_TOKEN_ID/SECRET or `~/.modal.toml`); the sandbox runs on
//! the user's Modal account and scales to zero when the run ends.

use std::collections::HashMap;

use crate::commands::exp::{hf_clone_script, spawn_detached_supervise};
use crate::error::{anyhow, Result};
use crate::jobs::{huggingface as hf, modal, BackendDescriptor};
use crate::local::git;
use crate::store::{now_ms, Store, StoredRun};

/// Modal app all orx sandboxes are grouped under (visible in the Modal dashboard).
const MODAL_APP: &str = "openresearch";

/// CLI wrapper around `submit_local_modal`: submit, then print the summary.
pub async fn launch_local_modal(args: &crate::ExpRunArgs) -> Result<()> {
    let run = submit_local_modal(args).await?;
    let backend = BackendDescriptor::parse(&run.backend_json)?;
    println!("\u{2713} Modal sandbox submitted.");
    println!(
        "  sandbox {} ({})",
        backend.job_id.as_deref().unwrap_or(""),
        backend.flavor.as_deref().unwrap_or("")
    );
    println!("  run     {}", run.id);
    println!(
        "  Follow it with `orx exp wait {}` or `orx logs {}`.",
        run.experiment_id, run.id
    );
    Ok(())
}

/// Submit the local experiment's run as a Modal Sandbox and detach a
/// supervisor. Requires `--backend modal` and `--flavor <name>` where the name
/// is a Modal GPU (t4, l4, a10g, a100, a100-80gb, l40s, h100, h200, …) or
/// `cpu` / `cpu-large` for CPU-only.
pub async fn submit_local_modal(args: &crate::ExpRunArgs) -> Result<StoredRun> {
    if args.sandbox.is_some() || args.gpu.is_some() || args.cpu.is_some() {
        return Err(anyhow!(
            "--backend modal runs on Modal serverless GPUs; drop --gpu/--cpu/--sandbox \
             and pass --flavor instead (e.g. --flavor a10g, --flavor a100-80gb, --flavor cpu)."
        ));
    }
    let flavor_name = args.flavor.clone().ok_or_else(|| {
        anyhow!(
            "--backend modal requires --flavor: a Modal GPU (t4, l4, a10g, a100, \
             a100-80gb, l40s, h100, h200, or e.g. h100:2 for a count) — or cpu / cpu-large \
             for CPU-only. Priced per second on your Modal account."
        )
    })?;
    let resources = modal::resolve_flavor(&flavor_name);
    // Fail before the git push if Modal plainly isn't set up on this box.
    modal::preflight().await?;
    // Same default as the HF/k8s paths — no-timeout jobs are a footgun.
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

    // The sandbox clones from GitHub, so the branch tip must exist there.
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
        .unwrap_or_else(|| modal::default_image(resources.gpu.is_some()));
    let script = hf_clone_script(
        &exp.branch_name,
        &project.github_owner,
        &project.github_repo,
        &run_command,
    );

    // The sandbox's env: everything the user synced (API keys), plus the tokens
    // the clone script and common tooling expect. Rides an ephemeral Modal
    // Secret, never the plain env arg.
    let mut env: HashMap<String, String> = crate::config::list_synced_env().into_iter().collect();
    if let Ok(hf_token) = hf::resolve_token() {
        env.entry("HF_TOKEN".to_string()).or_insert(hf_token);
    }
    if let Some(gh) = git::resolve_github_token() {
        env.insert("GITHUB_TOKEN".to_string(), gh);
    }
    let mut tags = HashMap::new();
    tags.insert("or_run".to_string(), run_id.clone());
    tags.insert("or_experiment".to_string(), exp.id.clone());
    tags.insert("or_project".to_string(), project.id.clone());

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

    let descriptor = BackendDescriptor {
        kind: "modal_job".to_string(),
        namespace: Some(MODAL_APP.to_string()),
        job_id: Some(sandbox_id),
        flavor: Some(flavor_name),
        image: Some(image),
        url: None,
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
