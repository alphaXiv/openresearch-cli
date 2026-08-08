//! Local Kubernetes launch — the k8s twin of `local/hf.rs`: the run row comes
//! from and goes to the local store only, and the detached `orx supervise`
//! watches the Job via kubectl. Cluster/namespace come from the settings file
//! (`orx up` Settings → Compute, or `~/.config/openresearch/k8s.json`).
//!
//! There are no flavors: the run's shape is a **manifest committed on the
//! experiment branch** (default `.orx/k8s.yaml`, or `--manifest <path>`),
//! read at the recorded revision — the same commit the job receives — so
//! uncommitted manifest edits never run. See `jobs/kubernetes.rs` for the contract orx
//! enforces on it.

use std::collections::HashMap;

use crate::commands::exp::spawn_detached_supervise;
use crate::compute::SourceSnapshot;
use crate::error::{anyhow, Result};
use crate::jobs::{huggingface as hf, kubernetes as k8s, BackendDescriptor};
use crate::local::git;
use crate::store::{now_ms, Store, StoredRun};

/// Manifest path on the experiment branch when `--manifest` is omitted.
const DEFAULT_MANIFEST: &str = ".orx/k8s.yaml";

/// CLI wrapper around `submit_local_k8s`: submit, then print the summary.
pub async fn launch_local_k8s(args: &crate::ExpRunArgs) -> Result<()> {
    let run = submit_local_k8s(args).await?;
    let backend = BackendDescriptor::parse(&run.backend_json)?;
    println!("\u{2713} Kubernetes resources created.");
    println!(
        "  job      {}/{}",
        backend.namespace.as_deref().unwrap_or(""),
        backend.job_id.as_deref().unwrap_or("")
    );
    println!(
        "  manifest {}",
        backend.manifest.as_deref().unwrap_or(DEFAULT_MANIFEST)
    );
    if let Some(resources) = backend.resources.as_deref().filter(|r| r.len() > 1) {
        println!("  created  {}", resources.join(", "));
    }
    println!("  run      {}", run.id);
    println!(
        "  Follow it with `orx exp wait {}` or `orx logs {}` (the log follows the \
         primary Job's leader pod).",
        run.experiment_id, run.id
    );
    Ok(())
}

/// Submit the local experiment's run from its committed manifest and detach a
/// supervisor.
pub async fn submit_local_k8s(args: &crate::ExpRunArgs) -> Result<StoredRun> {
    crate::compute::submit(args).await
}

pub async fn submit_local_k8s_with_source(
    args: &crate::ExpRunArgs,
    source: SourceSnapshot,
    run_id: String,
) -> Result<StoredRun> {
    if args.sandbox.is_some() || args.gpu.is_some() || args.cpu.is_some() {
        return Err(anyhow!(
            "--backend k8s runs from a manifest committed on the experiment branch; \
             drop --gpu/--cpu/--sandbox (resources live in the manifest)."
        ));
    }
    if args.flavor.is_some() {
        return Err(anyhow!(
            "--backend k8s has no flavors — the manifest on the experiment branch \
             (default {DEFAULT_MANIFEST}, or --manifest <path>) declares the resources."
        ));
    }
    if args.image.is_some() {
        return Err(anyhow!(
            "--image doesn't apply to --backend k8s — set the image in the manifest."
        ));
    }
    let settings = k8s::load_settings()?.unwrap_or_default();
    let manifest_path = args
        .manifest
        .clone()
        .unwrap_or_else(|| DEFAULT_MANIFEST.to_string());
    // Same default as the HF path — no-timeout jobs are a footgun. The
    // manifest's own activeDeadlineSeconds wins when set.
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
                 default with `orx project edit {} --run-command '<cmd>'`, or pass \
                 `--run-command '<cmd>'` to `orx create-experiment` — then relaunch.",
                project.id
            )
        })?;

    // One run in flight per experiment unless deliberately forced.
    let manifest = {
        let repo_path = project.repo_path.clone();
        let revision = source.revision.clone();
        let path = manifest_path.clone();
        tokio::task::spawn_blocking(move || -> Result<String> {
            git::file_at(std::path::Path::new(&repo_path), &revision, &path).map_err(|_| {
                    anyhow!(
                        "No manifest at '{path}' in committed revision {revision} — write one \
                         and commit it before running. Pass --manifest <path> if it lives elsewhere."
                    )
                })
        })
        .await
        .map_err(|e| anyhow!("git task failed: {e}"))??
    };

    let script = crate::compute::gated_script("/tmp/orx-source/source.tar", &run_command);

    // The pod's env: everything the user synced (API keys), plus the tokens
    // the run script and common tooling expect. Travels via a k8s Secret,
    // never on a command line.
    let mut env: HashMap<String, String> = crate::config::list_synced_env().into_iter().collect();
    if let Ok(hf_token) = hf::resolve_token() {
        env.entry("HF_TOKEN".to_string()).or_insert(hf_token);
    }
    let mut labels = HashMap::new();
    labels.insert("or_run".to_string(), run_id.clone());
    labels.insert("or_experiment".to_string(), exp.id.clone());
    labels.insert("or_project".to_string(), project.id.clone());

    // DNS-safe, run-unique token for {{ORX_RUN}} in resource names.
    let run_token = run_id
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(10)
        .collect::<String>();

    let context = settings.context.clone();
    let namespace = settings.namespace.clone();
    let submitted = k8s::run_manifest(
        context.as_deref(),
        &namespace,
        &k8s::ManifestSpec {
            manifest,
            script,
            run_token,
            env,
            timeout_seconds,
            labels,
            source_archive: source.path.clone(),
        },
    )
    .await?;

    let mut descriptor = BackendDescriptor {
        kind: "k8s_job".to_string(),
        namespace: Some(namespace.clone()),
        job_id: Some(submitted.job_name.clone()),
        flavor: None,
        image: None,
        url: None,
        context: context.clone(),
        manifest: Some(manifest_path),
        resources: Some(submitted.resources.clone()),
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
        let _ = k8s::delete_resources(context.as_deref(), &namespace, &submitted.resources).await;
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
