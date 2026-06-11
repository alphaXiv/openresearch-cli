//!
//! Creates a project in an organization. Two shapes, picked by flags:
//!   --repo <owner/repo>  -> bound to that GitHub repo (your own repo, or a
//!                           fresh copy when it's only readable)
//!   (no repo)            -> a fresh blank repo (stub root commit on `main`),
//!                           for projects that start from scratch
//! A name is always required.
//!
//! The project starts with no experiments. Create its baseline (root node)
//! afterwards with `orx create-experiment <projectId> --title "..."`.

use crate::client::{create_project, CreateProjectBody};
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
    println!();
    println!("The project has no experiments yet. Create its baseline (root node) with:");
    println!("  orx create-experiment {} --title \"<title>\"", project.id);
    Ok(())
}
