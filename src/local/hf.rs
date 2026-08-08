//! Local HF Jobs launch — mirrors `commands/exp.rs::launch_hf` with the api
//! calls deleted: the run row comes from and goes to the local store only.
//! The detached `orx supervise` it spawns detects local runs itself.

use std::collections::HashMap;

use crate::commands::exp::{default_hf_image, spawn_detached_supervise};
use crate::compute::SourceSnapshot;
use crate::error::{anyhow, Result};
use crate::jobs::{huggingface as hf, BackendDescriptor};
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
    crate::compute::submit(args).await
}

pub async fn submit_local_hf_with_source(
    args: &crate::ExpRunArgs,
    source: SourceSnapshot,
    run_id: String,
) -> Result<StoredRun> {
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
                 default with `orx project edit {} --run-command '<cmd>'`, or pass \
                 `--run-command '<cmd>'` to `orx create-experiment` — then relaunch.",
                project.id
            )
        })?;

    // One run in flight per experiment unless the caller deliberately forces
    // a concurrent launch — the double-click / double-submit guard.
    let token = hf::resolve_token()?;
    let namespace = hf::whoami(&token).await?;

    let image = args
        .image
        .clone()
        .unwrap_or_else(|| default_hf_image(&flavor));
    let script = crate::compute::snapshot_script("/orx-source/source.tar", &run_command);

    // Tokens travel as job secrets only — the command line stays tokenless.
    let mut secrets = HashMap::new();
    secrets.insert("HF_TOKEN".to_string(), token.clone());
    let mut labels = HashMap::new();
    labels.insert("or_run".to_string(), run_id.clone());
    labels.insert("or_experiment".to_string(), exp.id.clone());
    labels.insert("or_project".to_string(), project.id.clone());

    let job = hf::run_job_with_source(
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
        &source.path,
        &source.digest,
    )
    .await?;

    let mut descriptor = BackendDescriptor {
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
        source_digest: None,
        source_path: None,
        source_size: None,
    };
    source.apply_to_descriptor(&mut descriptor);
    if let Err(error) = crate::compute::record_submission_handle(&run_id, &descriptor) {
        let _ = hf::cancel_job(&token, &namespace, &job.id).await;
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
