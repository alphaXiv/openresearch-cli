//!
//! Creates an experiment node. Two shapes, picked by flags:
//!   --parent <id>   -> child experiment branched off that parent
//!   (no parent)     -> baseline (root) experiment on the project's bound repo
//! A title is always required.
//!
//! Note: the repo a project works on is chosen when the PROJECT is created
//! (`orx create-project` or the web), not here — so there is no `--repo` flag.
//! The baseline is materialized on whatever repo the project is already bound to.

use crate::client::{
    create_child_experiment, import_baseline, CreateChildBody, Experiment, ImportBaselineBody,
};
use crate::error::{anyhow, require_credentials, Result};
use crate::store::Store;

const USAGE: &str = "Usage: orx create-experiment <projectId> --title \"<title>\" [--parent <experimentId>] [--description \"<text>\"]";

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
        return run_local(&store, &project, title, args.parent, args.description);
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
        // branches `orx/<slug>` off the repo's default branch.
        let envelope = import_baseline(
            &creds,
            &args.project_id,
            &ImportBaselineBody {
                title: Some(title),
                description,
                generate_suggestions: None,
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

/// Local-mode create: child = branch `orx/<slug>` off the parent (pushed to
/// origin so HF jobs can clone it); no parent = a baseline root on the
/// project's baseline branch.
fn run_local(
    store: &Store,
    project: &crate::local::model::LocalProject,
    title: String,
    parent: Option<String>,
    description: Option<String>,
) -> Result<()> {
    let parent_exp = match &parent {
        Some(parent_id) => Some(store.get_local_experiment(parent_id)?.ok_or_else(|| {
            anyhow!(
                "Parent experiment {} not found in the local store. \
                 See `orx experiments` on the dashboard or omit --parent for a baseline.",
                parent_id
            )
        })?),
        None => None,
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
        None,
    )?;

    println!("\u{2713} Created local {} experiment", kind);
    println!("  id:      {}", experiment.id);
    println!("  title:   {}", experiment.display_name());
    println!("  slug:    {}", experiment.slug);
    println!("  branch:  {}", experiment.branch_name);
    if experiment.run_command.is_empty() {
        println!("  command: — (none inherited)");
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
