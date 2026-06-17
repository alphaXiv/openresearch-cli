//! The `project` command group: operate on a single project by id.
//!
//!   orx project edit <projectId> [--name …] [--description … | --description-stdin]
//!
//! Sibling to `orx projects` (which lists): the plural lists, the singular edits
//! one — mirroring `orx experiments` (list) vs `orx exp` (operate). Project ids
//! come from `orx projects`.

use tokio::io::AsyncReadExt;

use crate::client::{update_project, UpdateProjectBody};
use crate::error::{anyhow, require_credentials, Result};
use crate::ProjectCommand;

pub async fn run(args: crate::ProjectArgs) -> Result<()> {
    let creds = require_credentials().await;
    match args.command {
        ProjectCommand::Edit {
            project_id,
            name,
            description,
            description_stdin,
        } => edit(&creds, &project_id, name, description, description_stdin).await,
    }
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

    let res = update_project(
        creds,
        project_id,
        &UpdateProjectBody { name, description },
    )
    .await?;
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
