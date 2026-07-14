//! Local SSH launch — the SSH twin of `local/k8s.rs`: run the experiment as a
//! detached process on one of your own boxes over ssh. `--flavor` names an
//! `~/.ssh/config` host alias (there's no hardware scheduler on a plain
//! server). The run row lives in the local store only; a detached
//! `orx supervise` watches the remote process.

use std::collections::HashMap;

use crate::commands::exp::{hf_clone_script, spawn_detached_supervise};
use crate::error::{anyhow, Result};
use crate::jobs::{ssh, BackendDescriptor};
use crate::local::git;
use crate::store::{now_ms, Store, StoredRun};

/// CLI wrapper around `submit_local_ssh`: submit, then print the summary.
pub async fn launch_local_ssh(args: &crate::ExpRunArgs) -> Result<()> {
    let run = submit_local_ssh(args).await?;
    let backend = BackendDescriptor::parse(&run.backend_json)?;
    println!("\u{2713} SSH job started.");
    println!(
        "  host {}  ({})",
        backend.namespace.as_deref().unwrap_or(""),
        backend.job_id.as_deref().unwrap_or("")
    );
    println!("  run  {}", run.id);
    println!(
        "  Follow it with `orx exp wait {}` or `orx logs {}`.",
        run.experiment_id, run.id
    );
    Ok(())
}

/// Submit the local experiment's run as a detached process on an ssh host and
/// detach a supervisor. Requires `--backend ssh` and `--flavor <host>` where
/// the host is an `~/.ssh/config` alias.
pub async fn submit_local_ssh(args: &crate::ExpRunArgs) -> Result<StoredRun> {
    if args.sandbox.is_some() || args.gpu.is_some() || args.cpu.is_some() {
        return Err(anyhow!(
            "--backend ssh runs on your own box; drop --gpu/--cpu/--sandbox and pass \
             --host <alias> (an ~/.ssh/config alias) instead."
        ));
    }
    if args.flavor.is_some() {
        return Err(anyhow!(
            "--backend ssh has no flavors — a machine is an address, not a shape. \
             Pass --host <alias> (an ~/.ssh/config alias)."
        ));
    }
    if args.image.is_some() {
        return Err(anyhow!(
            "--image doesn't apply to --backend ssh — the run uses the host's own environment."
        ));
    }
    let host = args.host.clone().ok_or_else(|| {
        anyhow!(
            "--backend ssh requires --host <alias>: an ~/.ssh/config alias to run on \
             (see `orx up` Settings → Compute → SSH). The host needs git and bash."
        )
    })?;

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

    // The remote clones from GitHub, so the branch tip must exist there.
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
    let script = hf_clone_script(
        &exp.branch_name,
        &project.github_owner,
        &project.github_repo,
        &run_command,
    );

    // The remote env: everything the user synced (API keys), plus the tokens
    // the clone script expects. Exported inside run.sh (written owner-only).
    let mut env: HashMap<String, String> = crate::config::list_synced_env().into_iter().collect();
    if let Ok(hf_token) = crate::jobs::huggingface::resolve_token() {
        env.entry("HF_TOKEN".to_string()).or_insert(hf_token);
    }
    if let Some(gh) = git::resolve_github_token() {
        env.insert("GITHUB_TOKEN".to_string(), gh);
    }

    let remote_dir = ssh::run_job(&ssh::SshJobSpec {
        target: ssh::SshTarget::alias(&host),
        run_id: run_id.clone(),
        script,
        env,
    })
    .await?;

    let descriptor = BackendDescriptor {
        kind: "ssh_job".to_string(),
        namespace: Some(host),
        job_id: Some(remote_dir),
        flavor: None,
        image: None,
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
