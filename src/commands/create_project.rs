//!
//! Creates a project in an organization, then its baseline (root node) on the
//! bound repo — one command yields a project ready to hang experiments off.
//! Two shapes, picked by flags:
//!   --repo <owner/repo>  -> bound to that GitHub repo (your own repo, or a
//!                           fresh copy when it's only readable)
//!   (no repo)            -> a fresh blank repo (stub root commit on `main`),
//!                           for projects that start from scratch
//! A name is always required.
//!
//! The baseline import is a separate API call; if it fails after the project
//! was created, we print the `orx create-experiment` retry instead of failing
//! the whole command (so a re-run doesn't mint a duplicate project).

use crate::client::{create_project, import_baseline, CreateProjectBody, ImportBaselineBody};
use crate::error::{require_credentials, Result};

const USAGE: &str = "Usage: orx create-project <orgId> --name \"<name>\" [--repo <owner/repo|url>] [--branch <branch>] [--description \"<text>\"]";

pub async fn run(args: crate::CreateProjectArgs) -> Result<()> {
    let name = match args.name {
        Some(n) => n,
        None => {
            eprintln!("{}", USAGE);
            std::process::exit(1);
        }
    };
    if args.branch.is_some() && args.repo.is_none() {
        eprintln!("--branch only makes sense together with --repo.");
        eprintln!("{}", USAGE);
        std::process::exit(1);
    }

    let creds = require_credentials().await;
    let from_repo = args.repo.is_some();

    let result = create_project(
        &creds,
        &args.org_id,
        &CreateProjectBody {
            name,
            description: args.description,
            repo_full_name: args.repo,
            branch: args.branch,
        },
    )
    .await?;
    let project = result.project;

    let kind = if from_repo {
        "from repo"
    } else {
        "on a fresh blank repo"
    };
    println!("\u{2713} Created project {}", kind);
    println!("  id:   {}", project.id);
    println!("  name: {}", project.name);
    println!("  repo: {}/{}", project.github_owner, project.github_repo);

    // The root node: a baseline experiment on the repo we just bound.
    let baseline = import_baseline(
        &creds,
        &project.id,
        &ImportBaselineBody {
            title: None,
            description: None,
            generate_suggestions: None,
        },
    )
    .await;
    let experiment = match baseline {
        Ok(envelope) => envelope.experiment,
        Err(err) => {
            // The project exists; failing here would invite a duplicate-project
            // retry. Surface the recovery command instead.
            eprintln!();
            eprintln!(
                "Project created, but its baseline (root node) failed: {}",
                err
            );
            eprintln!("Create it with:");
            eprintln!("  orx create-experiment {} --title \"<title>\"", project.id);
            return Ok(());
        }
    };

    println!();
    println!("\u{2713} Created baseline (root node)");
    println!("  id:     {}", experiment.id);
    println!("  title:  {}", experiment.title);
    println!("  branch: {}", experiment.branch_name);
    println!();
    if from_repo {
        println!("Add child experiments off the baseline with:");
        println!(
            "  orx create-experiment {} --title \"<title>\" --parent {}",
            project.id, experiment.id
        );
    } else {
        println!("The baseline starts empty (a stub README). Push your starting code to it:");
        println!(
            "  git clone https://github.com/{}/{} && git checkout {}",
            project.github_owner, project.github_repo, experiment.branch_name
        );
        println!("  # …add code, commit, then…");
        println!("  git push -u origin {}", experiment.branch_name);
    }
    Ok(())
}
