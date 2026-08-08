//! Local launch — the on-this-machine twin of `local/ssh.rs`: run the
//! experiment as a detached process on the machine running orx. Same snapshot
//! contract as every backend — the run extracts the recorded revision into
//! its own run dir, never the agent's worktree. The run row lives in the
//! local store only; a detached `orx supervise` watches the process.

use std::collections::HashMap;

use crate::commands::exp::spawn_detached_supervise;
use crate::compute::SourceSnapshot;
use crate::error::{anyhow, Result};
use crate::jobs::{localbox, BackendDescriptor};
use crate::store::{now_ms, Store, StoredRun};

/// CLI wrapper around `submit_local_run`: submit, then print the summary.
pub async fn launch_local_run(args: &crate::ExpRunArgs) -> Result<()> {
    let run = submit_local_run(args).await?;
    let backend = BackendDescriptor::parse(&run.backend_json)?;
    println!("\u{2713} Local run started.");
    println!("  dir  {}", backend.job_id.as_deref().unwrap_or(""));
    println!("  run  {}", run.id);
    println!(
        "  Follow it with `orx exp wait {}` or `orx logs {}`.",
        run.experiment_id, run.id
    );
    Ok(())
}

/// Submit the local experiment's run as a detached process on this machine
/// and detach a supervisor. Requires `--backend local`; there is nothing else
/// to pick — the hardware is whatever this machine has.
pub async fn submit_local_run(args: &crate::ExpRunArgs) -> Result<StoredRun> {
    crate::compute::submit(args).await
}

pub async fn submit_local_run_with_source(
    args: &crate::ExpRunArgs,
    source: SourceSnapshot,
    run_id: String,
) -> Result<StoredRun> {
    if args.sandbox.is_some() || args.gpu.is_some() || args.cpu.is_some() {
        return Err(anyhow!(
            "--backend local runs on this machine; drop --gpu/--cpu/--sandbox — \
             there is nothing to provision."
        ));
    }
    if args.flavor.is_some() {
        return Err(anyhow!(
            "--backend local has no flavors — the hardware is whatever this machine has."
        ));
    }
    if args.image.is_some() {
        return Err(anyhow!(
            "--image doesn't apply to --backend local — the run uses this machine's \
             own environment."
        ));
    }

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
                 default with `orx project edit {} --run-command '<cmd>'`, or pass \
                 `--run-command '<cmd>'` to `orx create-experiment` — then relaunch.",
                project.id
            )
        })?;

    // One run in flight per experiment unless deliberately forced.
    let script = crate::compute::snapshot_script(&source.path.to_string_lossy(), &run_command);

    // The run's env: everything the user synced (API keys), plus the tokens
    // the run script expects. Exported inside run.sh (written owner-only).
    let mut env: HashMap<String, String> = crate::config::list_synced_env().into_iter().collect();
    if let Ok(hf_token) = crate::jobs::huggingface::resolve_token() {
        env.entry("HF_TOKEN".to_string()).or_insert(hf_token);
    }

    let dir = localbox::run_job(&localbox::LocalJobSpec {
        run_id: run_id.clone(),
        script,
        env,
    })?;

    let mut descriptor = BackendDescriptor {
        kind: "local_job".to_string(),
        namespace: None,
        job_id: Some(dir.to_string_lossy().into_owned()),
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
        source_digest: None,
        source_path: None,
        source_size: None,
    };
    source.apply_to_descriptor(&mut descriptor);
    if let Err(error) = crate::compute::record_submission_handle(&run_id, &descriptor) {
        let _ = localbox::cancel_job(&dir);
        return Err(error);
    }
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
        commit_sha: Some(source.revision),
        result_markdown: None,
        cancel_requested: store
            .get_run(&run_id)?
            .is_some_and(|run| run.cancel_requested),
        chat_session_id: crate::local::chat::launching_chat_session(),
    };
    store.upsert_run(&run)?;

    spawn_detached_supervise(&run_id)?;
    Ok(run)
}
