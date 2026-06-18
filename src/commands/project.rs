//! The `project` command group: operate on a single project by id.
//!
//!   orx project edit <projectId> [--name …] [--description … | --description-stdin]
//!
//! Sibling to `orx projects` (which lists): the plural lists, the singular edits
//! one — mirroring `orx experiments` (list) vs `orx exp` (operate). Project ids
//! come from `orx projects`.

use tokio::io::AsyncReadExt;

use crate::client::{
    get_project, list_experiments, list_reports, update_project, UpdateProjectBody,
};
use crate::commands::experiments::print_tree;
use crate::error::{anyhow, require_credentials, Result};
use crate::ProjectCommand;

pub async fn run(args: crate::ProjectArgs) -> Result<()> {
    let creds = require_credentials().await;
    match args.command {
        ProjectCommand::View { project_id } => view(&creds, &project_id).await,
        ProjectCommand::Edit {
            project_id,
            name,
            description,
            description_stdin,
        } => edit(&creds, &project_id, name, description, description_stdin).await,
    }
}

/// `orx project view <projectId>` — overview of a single project: its details,
/// experiment tree, and reports. Works for any public project, or any private
/// one in an org you belong to.
async fn view(creds: &crate::config::Credentials, project_id: &str) -> Result<()> {
    let project = get_project(creds, project_id).await?.project;

    println!("{}", project.name);
    println!("  id:     {}", project.id);
    if !project.github_owner.is_empty() {
        println!("  repo:   {}/{}", project.github_owner, project.github_repo);
    }
    println!(
        "  access: {}",
        if project.is_public {
            "public"
        } else {
            "private"
        }
    );
    if !project.description.is_empty() {
        println!("  about:  {}", project.description);
    }
    if let Some(q) = project
        .example_question
        .as_deref()
        .filter(|q| !q.is_empty())
    {
        println!("  ask:    {}", q);
    }

    let experiments = list_experiments(creds, project_id).await?.experiments;
    println!("\nExperiments");
    if experiments.is_empty() {
        println!("  (none)");
    } else {
        print_tree(&experiments);
    }

    let reports = list_reports(creds, project_id).await?.reports;
    println!("\nReports");
    if reports.is_empty() {
        println!("  (none)");
    } else {
        for r in &reports {
            println!("  {}  {}  ({})", r.id, r.title, r.created_at);
        }
        println!("\nRead one with: orx report show {} <reportId>", project_id);
    }
    Ok(())
}

/// `orx project edit <projectId> [--name …] [--description … | --description-stdin]`
/// — overwrite a project's name and/or description.
async fn edit(
    creds: &crate::config::Credentials,
    project_id: &str,
    name: Option<String>,
    description: Option<String>,
    description_stdin: bool,
) -> Result<()> {
    // `--description` and `--description-stdin` are mutually exclusive; either
    // present means "overwrite the description".
    let description = match (description, description_stdin) {
        (Some(_), true) => {
            return Err(anyhow!(
                "Pass either --description or --description-stdin, not both."
            ))
        }
        (Some(text), false) => Some(text),
        (None, true) => {
            let mut buf = String::new();
            tokio::io::stdin().read_to_string(&mut buf).await?;
            Some(buf)
        }
        (None, false) => None,
    };

    if name.is_none() && description.is_none() {
        return Err(anyhow!(
            "Nothing to change. Pass at least one of --name or --description \
             (or --description-stdin)."
        ));
    }

    let res = update_project(creds, project_id, &UpdateProjectBody { name, description }).await?;
    let project = res.project;

    println!("\u{2713} Project updated.");
    println!("  id:          {}", project.id);
    println!("  name:        {}", project.name);
    if project.description.is_empty() {
        println!("  description: — (empty)");
    } else {
        println!("  description: {}", project.description);
    }
    Ok(())
}
