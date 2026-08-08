//! Local Ray Jobs launch — submit via the Ray Jobs / Dashboard API, then
//! detach `orx supervise` to poll status and mirror logs.

use std::collections::HashMap;

use crate::commands::exp::spawn_detached_supervise;
use crate::compute::SourceSnapshot;
use crate::error::{anyhow, Result};
use crate::jobs::ssh::sh_quote;
use crate::jobs::{huggingface, ray, BackendDescriptor};
use crate::store::{now_ms, Store, StoredRun};

/// CLI wrapper: submit, then print the summary.
pub async fn launch_local_ray(args: &crate::ExpRunArgs) -> Result<()> {
    let run = submit_local_ray(args).await?;
    let backend = BackendDescriptor::parse(&run.backend_json)?;
    println!("\u{2713} Ray job submitted.");
    println!("  run    {}", run.id);
    println!(
        "  job    {} ({})",
        backend.job_id.as_deref().unwrap_or(""),
        backend.flavor.as_deref().unwrap_or("default")
    );
    println!("  watch  {}", backend.url.as_deref().unwrap_or(""));
    println!(
        "  Follow it with `orx exp wait {}` or `orx logs {}`.",
        run.experiment_id, run.id
    );
    Ok(())
}

/// Submit the local experiment's run as a Ray Job and detach a supervisor.
pub async fn submit_local_ray(args: &crate::ExpRunArgs) -> Result<StoredRun> {
    crate::compute::submit(args).await
}

pub async fn submit_local_ray_with_source(
    args: &crate::ExpRunArgs,
    source: SourceSnapshot,
    run_id: String,
) -> Result<StoredRun> {
    if args.sandbox.is_some() || args.gpu.is_some() || args.cpu.is_some() {
        return Err(anyhow!(
            "--backend ray submits to your Ray cluster; drop --gpu/--cpu/--sandbox and \
             ask for resources with --flavor (e.g. --flavor gpu:1)."
        ));
    }
    if args.image.is_some() {
        return Err(anyhow!(
            "--image doesn't apply to --backend ray — the job runs in the cluster's \
             runtime environment, not a per-job container."
        ));
    }
    if args.host.is_some() {
        return Err(anyhow!(
            "--host only applies with --backend ssh/slurm. Save the Ray Jobs URL in \
             OpenResearch or set ASTROAI_RAY_JOBS_ADDRESS / RAY_DASHBOARD_URL."
        ));
    }
    if args.manifest.is_some() {
        return Err(anyhow!("--manifest only applies with --backend k8s."));
    }
    if args.timeout.is_some() {
        return Err(anyhow!(
            "--timeout isn't supported on --backend ray — Ray Jobs have no time limit; \
             the job runs until the command exits. Bound the run in the command itself."
        ));
    }

    let resources = ray::parse_flavor(args.flavor.as_deref())?;
    let address = ray::resolve_address(None);

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

    // Reachability check before we touch git / allocate a run id.
    ray::preflight(&address).await.map_err(|e| {
        anyhow!(
            "{e}\n\
             Save the Jobs URL in OpenResearch, or export \
             ASTROAI_RAY_JOBS_ADDRESS / RAY_DASHBOARD_URL."
        )
    })?;

    // Ray submission ids: letters, digits, dashes, underscores.
    let submission_id = format!("orx-{}", run_id.replace('-', ""));
    // The job env: everything the user synced (API keys), plus the tokens the
    // run step expects. Ray renders runtime_env in its dashboard, but anyone
    // with dashboard access can submit jobs anyway — same trust boundary.
    let mut env: HashMap<String, String> = crate::config::list_synced_env().into_iter().collect();
    if let Ok(hf_token) = huggingface::resolve_token() {
        env.entry("HF_TOKEN".to_string()).or_insert(hf_token);
    }
    let mut metadata = HashMap::new();
    metadata.insert("or_run".to_string(), run_id.clone());
    metadata.insert("or_experiment".to_string(), exp.id.clone());
    metadata.insert("or_project".to_string(), project.id.clone());

    let (package_digest, package_path) = source
        .ray_package
        .as_ref()
        .ok_or_else(|| anyhow!("Ray source package was not created."))?;
    let working_dir = ray::stage_working_dir(&address, package_digest, package_path).await?;
    ray::run_job(
        &address,
        &ray::JobSubmission {
            entrypoint: format!("bash -c {}", sh_quote(&run_command)),
            submission_id: submission_id.clone(),
            resources,
            env,
            metadata,
            working_dir: Some(working_dir),
        },
    )
    .await?;

    let watch = ray::job_url(&address, &submission_id);

    let mut descriptor = BackendDescriptor {
        kind: "ray_job".to_string(),
        namespace: Some(address.clone()),
        job_id: Some(submission_id.clone()),
        flavor: args.flavor.clone(),
        image: None,
        url: Some(watch),
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
        let _ = ray::stop_job(&address, &submission_id).await;
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
