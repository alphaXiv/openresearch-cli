//!
//! Creates an experiment node. Shapes, picked by flags:
//!   --parent <id>   -> child experiment branched off that parent
//!   --baseline      -> a new baseline (root), even when roots already exist —
//!                      projects may hold multiple baselines
//!   (no flags)      -> the oldest project root when one exists (local
//!                      projects), or the baseline (root) when the tree is
//!                      empty — projects start with no experiments, so the
//!                      first create is the baseline: the control its
//!                      variants are measured against
//! A title is always required.
//!
//! Note: the repo a project works on is chosen when the PROJECT is created
//! (`orx create-project` or the web), not here — so there is no `--repo` flag.
//! The baseline is materialized on whatever repo the project is already bound to.

use crate::client::{
    create_baseline_experiment, create_child_experiment, CreateBaselineExperimentBody,
    CreateChildBody, Experiment,
};
use crate::error::{anyhow, require_credentials, Result};
use crate::store::Store;

const USAGE: &str = "Usage: orx create-experiment <projectId> --title \"<title>\" [--parent <experimentId>] [--description \"<text>\"] [--run-command \"<cmd>\"]";

pub async fn run(args: crate::CreateExperimentArgs) -> Result<()> {
    let title = match args.title {
        Some(t) => t,
        None => {
            eprintln!("{}", USAGE);
            std::process::exit(1);
        }
    };

    // Local project (orx up): create the row + branch locally, no api.
    let store = Store::open()?;
    if let Some(project) = store.get_local_project(&args.project_id)? {
        return run_local(
            &store,
            &project,
            title,
            args.parent,
            args.baseline,
            args.description,
            args.run_command,
        );
    }

    // The server child-create API carries no run command field — refuse rather
    // than silently drop it. (The baseline create below does accept one.)
    if args.run_command.is_some() && args.parent.is_some() {
        return Err(anyhow!(
            "--run-command is supported for local projects and server baselines \
             only. For server child experiments, set it after creation with \
             `orx exp cmd <expId> --set '<cmd>'`."
        ));
    }

    let creds = require_credentials().await;
    let description = args.description;

    let experiment: Experiment;
    let kind: String;
    if let Some(parent) = args.parent {
        let envelope = create_child_experiment(
            &creds,
            &args.project_id,
            &CreateChildBody {
                title,
                description,
                parent_experiment_id: parent,
            },
        )
        .await?;
        experiment = envelope.experiment;
        kind = "child".to_string();
    } else {
        // Baseline on the project's already-bound GitHub repo. The server
        // branches `orx/<slug>` off the branch picked at project creation
        // (the repo's default unless one was chosen).
        let envelope = create_baseline_experiment(
            &creds,
            &args.project_id,
            &CreateBaselineExperimentBody {
                title: Some(title),
                description,
                run_command: args.run_command,
            },
        )
        .await?;
        experiment = envelope.experiment;
        kind = "baseline".to_string();
    }

    println!("\u{2713} Created {} experiment", kind);
    println!("  id:     {}", experiment.id);
    println!("  title:  {}", experiment.title);
    println!("  slug:   {}", experiment.slug);
    println!("  branch: {}", experiment.branch_name);
    println!();
    println!("To edit it, check out the branch in your local clone of the project's repo:");
    println!(
        "  git fetch origin && git checkout {}",
        experiment.branch_name
    );
    println!("  # …edit, then…");
    println!(
        "  git commit -am \"<msg>\" && git push -u origin {}",
        experiment.branch_name
    );
    Ok(())
}

/// Local-mode create: every node gets a branch `orx/<slug>` pushed to origin
/// so jobs can clone it — children fork off the parent's tip, baselines off
/// the project's base branch (which itself is never an experiment node). No
/// parent = child of the project's oldest root when one exists; on an empty
/// project (or with `--baseline`) the new row becomes a baseline root.
/// Projects may hold multiple baselines.
fn run_local(
    store: &Store,
    project: &crate::local::model::LocalProject,
    title: String,
    parent: Option<String>,
    baseline: bool,
    description: Option<String>,
    run_command: Option<String>,
) -> Result<()> {
    let mut defaulted_to_root = false;
    let parent_exp = match &parent {
        Some(parent_id) => Some(store.get_local_experiment(parent_id)?.ok_or_else(|| {
            anyhow!(
                "Parent experiment {} not found in the local store. \
                 See the dashboard, or omit --parent to branch off the project root.",
                parent_id
            )
        })?),
        None if baseline => None,
        None => {
            let root = crate::local::experiments::project_root(store, &project.id)?;
            defaulted_to_root = root.is_some();
            root
        }
    };
    let kind = if parent_exp.is_some() {
        "child"
    } else {
        "baseline"
    };

    let experiment = crate::local::experiments::create_experiment(
        store,
        project,
        parent_exp.as_ref(),
        None,
        Some(title),
        description,
        run_command,
    )?;

    println!("\u{2713} Created local {} experiment", kind);
    if defaulted_to_root {
        let root = parent_exp.as_ref().unwrap();
        println!("  parent:  {} (project root, defaulted)", root.id);
    }
    if let Some(warning) = parent_exp
        .as_ref()
        .and_then(|p| crate::local::experiments::legacy_root_warning(project, p))
    {
        eprintln!("  {warning}");
    }
    println!("  id:      {}", experiment.id);
    println!("  title:   {}", experiment.display_name());
    println!("  slug:    {}", experiment.slug);
    println!("  branch:  {}", experiment.branch_name);
    if experiment.run_command.is_empty() {
        println!(
            "  command: — (none inherited — set one with `orx project edit {} --run-command '<cmd>'`)",
            project.id
        );
    } else {
        println!("  command: {}", experiment.run_command);
    }
    println!();
    println!("To edit it, check out the branch in the project's local clone:");
    println!("  cd {}", project.repo_path);
    println!(
        "  git fetch origin && git checkout {}",
        experiment.branch_name
    );
    println!("  # …edit, then…");
    println!(
        "  git commit -am \"<msg>\" && git push -u origin {}",
        experiment.branch_name
    );
    Ok(())
}
