//! Local Slurm launch — the scheduler-backed twin of `local/ssh.rs`: submit
//! the experiment as a batch job on a Slurm cluster reached via its login
//! node. `--host` names an `~/.ssh/config` alias (defaultable in the slurm
//! settings); `--flavor` asks for GPUs as a GRES spec. The run row lives in
//! the local store only; a detached `orx supervise` watches the job.

use std::collections::HashMap;

use crate::commands::exp::spawn_detached_supervise;
use crate::compute::SourceSnapshot;
use crate::error::{anyhow, Result};
use crate::jobs::{huggingface, slurm, BackendDescriptor};
use crate::store::{now_ms, Store, StoredRun};

/// CLI wrapper around `submit_local_slurm`: submit, then print the summary.
pub async fn launch_local_slurm(args: &crate::ExpRunArgs) -> Result<()> {
    let run = submit_local_slurm(args).await?;
    let backend = BackendDescriptor::parse(&run.backend_json)?;
    println!("\u{2713} Slurm job submitted.");
    println!(
        "  host {}  (job {})",
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

/// Submit the local experiment's run as a Slurm batch job and detach a
/// supervisor. Requires `--backend slurm`; the login node comes from
/// `--host <alias>` or the slurm settings default.
pub async fn submit_local_slurm(args: &crate::ExpRunArgs) -> Result<StoredRun> {
    crate::compute::submit(args).await
}

pub async fn submit_local_slurm_with_source(
    args: &crate::ExpRunArgs,
    source: SourceSnapshot,
    run_id: String,
) -> Result<StoredRun> {
    if args.sandbox.is_some() || args.gpu.is_some() || args.cpu.is_some() {
        return Err(anyhow!(
            "--backend slurm runs on your own cluster; drop --gpu/--cpu/--sandbox and \
             ask for GPUs with --flavor (e.g. --flavor h100:2)."
        ));
    }
    if args.image.is_some() {
        return Err(anyhow!(
            "--image doesn't apply to --backend slurm — the job runs in your cluster \
             environment (modules/conda), not a container."
        ));
    }
    if args.manifest.is_some() {
        return Err(anyhow!("--manifest only applies with --backend k8s."));
    }
    // A muscle-memory `--flavor cpu` (HF/Modal habit) would become the
    // nonsense GRES `gpu:cpu` and die with an opaque sbatch error.
    if let Some(f) = &args.flavor {
        if f.trim().to_ascii_lowercase().starts_with("cpu") {
            return Err(anyhow!(
                "--flavor names GPUs on --backend slurm (e.g. h100:2). For a CPU-only \
                 run just omit --flavor; CPUs come from the partition defaults."
            ));
        }
    }

    let settings = slurm::load_settings()?.unwrap_or_default();
    let host = args
        .host
        .clone()
        .or_else(|| settings.host.clone())
        .ok_or_else(|| {
            anyhow!(
                "--backend slurm needs a login node: pass --host <alias> (an ~/.ssh/config \
                 alias) or use a configured default Slurm host."
            )
        })?;

    // `--timeout` beats the settings default; neither = the cluster's default.
    let time_limit_secs = match args.timeout.as_deref().or(settings.time_limit.as_deref()) {
        Some(t) => Some(huggingface::parse_timeout(t)?),
        None => None,
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
                 default with `orx project edit {} --run-command '<cmd>'`, or pass \
                 `--run-command '<cmd>'` to `orx create-experiment` — then relaunch.",
                project.id
            )
        })?;

    // One run in flight per experiment unless deliberately forced.
    // The job env: everything the user synced (API keys), plus the tokens the
    // run step expects. Exported in the setup script and job.sbatch.
    let mut env: HashMap<String, String> = crate::config::list_synced_env().into_iter().collect();
    if let Ok(hf_token) = huggingface::resolve_token() {
        env.entry("HF_TOKEN".to_string()).or_insert(hf_token);
    }
    crate::jobs::ssh::stage_source(
        &crate::jobs::ssh::SshTarget::alias(&host),
        &run_id,
        &source.path,
        &source.digest,
    )
    .await?;
    let job_id = slurm::run_job(&slurm::SlurmJobSpec {
        host: host.clone(),
        run_id: run_id.clone(),
        setup_script: "test -d repo".to_string(),
        command: run_command.clone(),
        env,
        gres: args.flavor.as_deref().and_then(slurm::resolve_gres),
        partition: settings.partition.clone(),
        account: settings.account.clone(),
        time_limit_secs,
    })
    .await?;

    let mut descriptor = BackendDescriptor {
        kind: "slurm_job".to_string(),
        namespace: Some(host.clone()),
        job_id: Some(job_id.clone()),
        flavor: args.flavor.clone(),
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
        let _ = slurm::cancel_job(&host, &job_id).await;
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
