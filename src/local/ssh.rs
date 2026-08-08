//! Local SSH launch — the SSH twin of `local/k8s.rs`: run the experiment as a
//! detached process on one of your own boxes over ssh. `--flavor` names an
//! `~/.ssh/config` host alias (there's no hardware scheduler on a plain
//! server). The run row lives in the local store only; a detached
//! `orx supervise` watches the remote process.

use std::collections::HashMap;

use crate::commands::exp::spawn_detached_supervise;
use crate::compute::SourceSnapshot;
use crate::error::{anyhow, Result};
use crate::jobs::{ssh, BackendDescriptor};
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
    crate::compute::submit(args).await
}

pub async fn submit_local_ssh_with_source(
    args: &crate::ExpRunArgs,
    source: SourceSnapshot,
    run_id: String,
) -> Result<StoredRun> {
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
            "--backend ssh requires --host <alias> from the user's ~/.ssh/config. \
             The host needs git and bash."
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
                 default with `orx project edit {} --run-command '<cmd>'`, or pass \
                 `--run-command '<cmd>'` to `orx create-experiment` — then relaunch.",
                project.id
            )
        })?;

    // One run in flight per experiment unless deliberately forced.
    let target = ssh::SshTarget::alias(&host);
    ssh::stage_source(&target, &run_id, &source.path, &source.digest).await?;
    let script = crate::compute::staged_script(&run_command);

    // The remote env: everything the user synced (API keys), plus the tokens
    // the run script expects. Exported inside run.sh (written owner-only).
    let mut env: HashMap<String, String> = crate::config::list_synced_env().into_iter().collect();
    if let Ok(hf_token) = crate::jobs::huggingface::resolve_token() {
        env.entry("HF_TOKEN".to_string()).or_insert(hf_token);
    }

    let remote_dir = ssh::run_job(&ssh::SshJobSpec {
        target: target.clone(),
        run_id: run_id.clone(),
        script,
        env,
    })
    .await?;

    let mut descriptor = BackendDescriptor {
        kind: "ssh_job".to_string(),
        namespace: Some(host),
        job_id: Some(remote_dir.clone()),
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
        let _ = ssh::cancel_job(&target, &remote_dir).await;
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
