//! Local OpenResearch launch — provision an ephemeral org-billed box for this
//! one run. Unlike `local/ssh.rs`, submit does NOT launch the payload: the box
//! takes minutes to provision and has no SSH endpoint yet, so submit records
//! the run as `starting` and the detached supervisor does the rest (wait for
//! online, launch over ssh, watch, tear the box down at terminal state).

use crate::client::{create_sandbox, list_orgs, list_ssh_keys, CreateSandboxBody};
use crate::commands::exp::spawn_detached_supervise;
use crate::error::{anyhow, require_credentials, Result};
use crate::jobs::{openresearch, BackendDescriptor};
use crate::local::git;
use crate::store::{now_ms, Store, StoredRun};

/// CLI wrapper around `submit_local_openresearch`: submit, then print the
/// summary (price is best-effort — it needs one extra API read).
pub async fn launch_local_openresearch(args: &crate::ExpRunArgs) -> Result<()> {
    let run = submit_local_openresearch(args).await?;
    let backend = BackendDescriptor::parse(&run.backend_json)?;
    let sandbox_id = backend.job_id.as_deref().unwrap_or("");
    println!("\u{2713} OpenResearch box requested.");
    println!("  flavor  {}", backend.flavor.as_deref().unwrap_or(""));
    println!("  box     {}", sandbox_id);
    if let Ok(Some(creds)) = crate::config::load_credentials().await {
        if let Ok(envelope) = crate::client::get_sandbox(&creds, sandbox_id).await {
            if let Some(price) = envelope.sandbox.price_per_hour {
                println!("  price   ${price:.2}/hr (until the run ends)");
            }
        }
    }
    println!("  run     {}", run.id);
    println!("  The box is provisioning; the supervisor launches the run once it's online.");
    println!(
        "  Follow it with `orx exp wait {}` or `orx logs {}`.",
        run.experiment_id, run.id
    );
    Ok(())
}

/// Provision a fresh box for the local experiment's run and detach a
/// supervisor. Requires `--backend openresearch` and `--flavor <shape>`
/// (`h100_sxm[:count]` or `cpu5c|cpu5g|cpu5m[:vcpus]`), plus `orx login`.
pub async fn submit_local_openresearch(args: &crate::ExpRunArgs) -> Result<StoredRun> {
    if args.sandbox.is_some() || args.gpu.is_some() || args.cpu.is_some() {
        return Err(anyhow!(
            "--gpu/--cpu/--sandbox are the managed server-experiment flags; with \
             --backend openresearch pass the shape as --flavor (e.g. --flavor h100_sxm \
             or --flavor cpu5c)."
        ));
    }
    if args.host.is_some() {
        return Err(anyhow!(
            "--host doesn't apply to --backend openresearch — the box is provisioned \
             for you (use --backend ssh to run on your own machine)."
        ));
    }
    if args.manifest.is_some() {
        return Err(anyhow!("--manifest is k8s-only."));
    }
    if args.image.is_some() {
        return Err(anyhow!(
            "--image doesn't apply to --backend openresearch — boxes run the platform's \
             fixed image (CUDA + PyTorch + uv preinstalled)."
        ));
    }
    let flavor = args.flavor.clone().ok_or_else(|| {
        anyhow!(
            "--backend openresearch requires --flavor: a GPU id like h100_sxm[:count] \
             or a CPU flavor like cpu5c[:vcpus]. See `orx compute` for the catalog."
        )
    })?;
    let target =
        openresearch::parse_flavor(&flavor, args.disk.unwrap_or(100), args.provider.clone())?;
    // Enforced later (the supervisor wraps the payload) — parsed now so a
    // typo fails before any billing starts. Same 4h default as hf/modal.
    let timeout_secs = match &args.timeout {
        Some(t) => crate::jobs::huggingface::parse_timeout(t)?,
        None => 4 * 3600,
    };

    let creds = require_credentials().await;

    // Which org pays for the box.
    let org_id = match &args.org {
        Some(org) => org.clone(),
        None => {
            let orgs = list_orgs(&creds).await?.orgs;
            match orgs.len() {
                0 => {
                    return Err(anyhow!(
                        "You belong to no org — create one in the dashboard."
                    ))
                }
                1 => orgs[0].id.clone(),
                _ => {
                    let rows = orgs
                        .iter()
                        .map(|o| format!("  {}  {}", o.id, o.name))
                        .collect::<Vec<_>>()
                        .join("\n");
                    return Err(anyhow!(
                        "You belong to several orgs — pass --org <id>:\n{rows}"
                    ));
                }
            }
        }
    };

    // Boxes authorize org members' *registered* SSH keys; without one the box
    // would come online unreachable. Preflight is best-effort — an older API
    // without the route must not block the submit.
    match list_ssh_keys(&creds).await {
        Ok(keys) if keys.ssh_keys.is_empty() => {
            return Err(anyhow!(
                "No SSH key is registered on your account, so orx couldn't reach the box. \
                 Add one in the dashboard (Settings → SSH keys), then rerun."
            ));
        }
        Ok(_) => {}
        Err(err) => eprintln!("warning: could not check your registered SSH keys: {err}"),
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

    // The box clones from GitHub, so the branch tip must exist there — push
    // BEFORE provisioning so a git failure never bills a box.
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

    let sandbox = create_sandbox(
        &creds,
        &CreateSandboxBody {
            organization_id: org_id.clone(),
            target,
        },
    )
    .await
    .map_err(billing_friendly)?
    .sandbox;

    let run_id = uuid::Uuid::new_v4().to_string();
    let descriptor = BackendDescriptor {
        kind: "openresearch_job".to_string(),
        namespace: Some(org_id),
        job_id: Some(sandbox.id.clone()),
        flavor: Some(flavor),
        image: None,
        url: None,
        context: None,
        manifest: None,
        resources: None,
        ssh_host: None,
        ssh_port: None,
        ssh_user: None,
        timeout_secs: Some(timeout_secs),
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
        chat_session_id: crate::local::chat::launching_chat_session(),
    };

    // From here the box is billing: never leak it behind an error the store
    // doesn't know about.
    let persisted = store
        .upsert_run(&run)
        .and_then(|()| spawn_detached_supervise(&run_id));
    if let Err(err) = persisted {
        eprintln!(
            "submit failed after the box was provisioned — deleting box {}",
            sandbox.id
        );
        if let Err(td) = openresearch::teardown(&creds, &sandbox.id).await {
            eprintln!(
                "warning: box {} could not be torn down ({td}) — delete it with \
                 `orx instance delete {}` or from the dashboard.",
                sandbox.id, sandbox.id
            );
        }
        return Err(err);
    }
    Ok(run)
}

/// Reword a 402 from `POST /sandboxes` (org has no available compute balance)
/// into the top-up action, surfacing the checkout URL the API attaches.
fn billing_friendly(err: crate::error::Error) -> crate::error::Error {
    let text = err.to_string();
    if !text.contains("(402 ") {
        return err;
    }
    let url = text
        .split("checkoutUrl\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .map(|u| format!(" Top up at {u}"))
        .unwrap_or_default();
    anyhow!("Your org has no available compute balance, so no box was provisioned.{url}")
}
