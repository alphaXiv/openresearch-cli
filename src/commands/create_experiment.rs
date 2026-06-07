//!
//! Creates an experiment node. Three shapes, picked by flags:
//!   --parent <id>   -> child experiment branched off that parent
//!   --repo a/b      -> root experiment imported from a GitHub repo
//!   (neither)       -> empty root experiment
//! A title is always required.

use crate::client::{
    create_child_experiment, create_empty_baseline, import_baseline, CreateChildBody,
    CreateEmptyBaselineBody, Experiment, ImportBaselineBody,
};
use crate::error::{require_credentials, Result};

const USAGE: &str = "Usage: orx create-experiment <projectId> --title \"<title>\" [--parent <experimentId>] [--repo <owner/repo> [--ref <ref>]] [--description \"<text>\"]";

pub async fn run(args: crate::CreateExperimentArgs) -> Result<()> {
    let title = match args.title {
        Some(t) => t,
        None => {
            eprintln!("{}", USAGE);
            std::process::exit(1);
        }
    };

    if args.parent.is_some() && args.repo.is_some() {
        eprintln!("Choose one of --parent or --repo, not both.");
        eprintln!("(--parent makes a child node; --repo makes a root node from a git repo.)");
        std::process::exit(1);
    }
    if args.ref_.is_some() && args.repo.is_none() {
        eprintln!("--ref only applies together with --repo.");
        std::process::exit(1);
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
    } else if let Some(repo) = args.repo {
        // The repo must be a GitHub repo ("owner/repo") reachable through the
        // org's GitHub App installation -- it's imported via tarball, not an
        // arbitrary `git clone` URL. `patch` is required by the endpoint; we
        // send null.
        let envelope = import_baseline(
            &creds,
            &args.project_id,
            &ImportBaselineBody {
                repo_full_name: repo.clone(),
                ref_: args.ref_.unwrap_or_default(),
                patch: None,
                title,
                description,
            },
        )
        .await?;
        experiment = envelope.experiment;
        kind = format!("root (from {})", repo);
    } else {
        let envelope = create_empty_baseline(
            &creds,
            &args.project_id,
            &CreateEmptyBaselineBody { title, description },
        )
        .await?;
        experiment = envelope.experiment;
        kind = "root (empty)".to_string();
    }

    println!("\u{2713} Created {} experiment", kind);
    println!("  id:    {}", experiment.id);
    println!("  title: {}", experiment.title);
    println!("  slug:  {}", experiment.slug);
    Ok(())
}
