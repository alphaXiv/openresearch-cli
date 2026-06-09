//!
//! Creates an experiment node. Two shapes, picked by flags:
//!   --parent <id>   -> child experiment branched off that parent
//!   (no parent)     -> baseline (root) experiment on the project's bound repo
//! A title is always required.
//!
//! Note: the repo a project works on is chosen when the PROJECT is created (on
//! the web), not here — so there is no longer a `--repo` flag. The baseline is
//! materialized on whatever repo the project is already bound to.

use crate::client::{
    create_child_experiment, import_baseline, CreateChildBody, Experiment, ImportBaselineBody,
};
use crate::error::{require_credentials, Result};

const USAGE: &str = "Usage: orx create-experiment <projectId> --title \"<title>\" [--parent <experimentId>] [--description \"<text>\"]";

pub async fn run(args: crate::CreateExperimentArgs) -> Result<()> {
    let title = match args.title {
        Some(t) => t,
        None => {
            eprintln!("{}", USAGE);
            std::process::exit(1);
        }
    };

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
    println!("  id:    {}", experiment.id);
    println!("  title: {}", experiment.title);
    println!("  slug:  {}", experiment.slug);
    Ok(())
}
