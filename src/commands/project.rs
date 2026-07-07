//! The `project` command group: operate on a single project by id.
//!
//!   orx project edit <projectId> [--name …] [--description … | --description-stdin] [--public | --private]
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
    let project_id = match &args.command {
        ProjectCommand::View { project_id } | ProjectCommand::Edit { project_id, .. } => project_id,
    };
    let store = crate::store::Store::open()?;
    if let Some(project) = store.get_local_project(project_id)? {
        return match args.command {
            ProjectCommand::View { .. } => view_local(&store, &project),
            ProjectCommand::Edit {
                name,
                description,
                description_stdin,
                public,
                private,
                run_command,
                ..
            } => {
                if description.is_some() || description_stdin || public || private {
                    return Err(anyhow!(
                        "Local projects support --name and --run-command only."
                    ));
                }
                edit_local(&store, project, name, run_command)
            }
        };
    }
    // The server project PATCH carries no run command field — refuse before
    // even asking for credentials.
    if let ProjectCommand::Edit {
        run_command: Some(_),
        ..
    } = &args.command
    {
        return Err(anyhow!(
            "--run-command is supported for local projects only. For server \
             projects, set it per experiment with `orx exp cmd <expId> --set '<cmd>'`."
        ));
    }
    let creds = require_credentials().await;
    match args.command {
        ProjectCommand::View { project_id } => view(&creds, &project_id).await,
        ProjectCommand::Edit {
            project_id,
            name,
            description,
            description_stdin,
            public,
            private,
            run_command: _,
        } => {
            edit(
                &creds,
                &project_id,
                name,
                description,
                description_stdin,
                public,
                private,
            )
            .await
        }
    }
}

/// Local `orx project view`: the project row, its default run command, and a
/// flat experiment list (there is no local `orx experiments`).
fn view_local(
    store: &crate::store::Store,
    project: &crate::local::model::LocalProject,
) -> Result<()> {
    println!("{} (local)", project.name);
    println!("  id:      {}", project.id);
    println!(
        "  repo:    {}/{} (baseline branch: {})",
        project.github_owner, project.github_repo, project.baseline_branch
    );
    println!("  clone:   {}", project.repo_path);
    match project
        .run_command
        .as_deref()
        .filter(|c| !c.trim().is_empty())
    {
        Some(cmd) => println!("  command: {}", cmd),
        None => println!(
            "  command: — (not set — `orx project edit {} --run-command '<cmd>'`)",
            project.id
        ),
    }

    let experiments = store.list_experiments_by_project(&project.id)?;
    println!("\nExperiments");
    if experiments.is_empty() {
        println!("  (none)");
    } else {
        for e in &experiments {
            let root = if e.parent_experiment_id.is_none() {
                " [root]"
            } else {
                ""
            };
            println!(
                "  {}  {}{}  ({})",
                e.id,
                e.display_name(),
                root,
                e.branch_name
            );
        }
    }
    Ok(())
}

/// Local `orx project edit`: rename and/or set the default run command
/// (`--run-command ''` clears it). New experiments inherit the command.
fn edit_local(
    store: &crate::store::Store,
    mut project: crate::local::model::LocalProject,
    name: Option<String>,
    run_command: Option<String>,
) -> Result<()> {
    if name.is_none() && run_command.is_none() {
        return Err(anyhow!(
            "Nothing to change. Pass at least one of --name or --run-command."
        ));
    }
    if let Some(name) = name {
        if name.trim().is_empty() {
            return Err(anyhow!("--name cannot be empty."));
        }
        project.name = name.trim().to_string();
    }
    if let Some(cmd) = run_command {
        project.run_command = Some(cmd).filter(|c| !c.trim().is_empty());
    }
    store.update_local_project(&project)?;

    println!("\u{2713} Project updated.");
    println!("  id:      {}", project.id);
    println!("  name:    {}", project.name);
    match project.run_command.as_deref() {
        Some(cmd) => println!("  command: {}", cmd),
        None => println!("  command: — (empty)"),
    }
    Ok(())
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

/// `orx project edit <projectId> [--name …] [--description … | --description-stdin] [--public | --private]`
/// — overwrite a project's name, description, and/or visibility.
async fn edit(
    creds: &crate::config::Credentials,
    project_id: &str,
    name: Option<String>,
    description: Option<String>,
    description_stdin: bool,
    public: bool,
    private: bool,
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

    // `--public` / `--private` map to the `isPublic` flag; clap's
    // `conflicts_with` already rejects passing both. Neither flag leaves
    // visibility untouched (`None`).
    let is_public = match (public, private) {
        (true, false) => Some(true),
        (false, true) => Some(false),
        _ => None,
    };

    if name.is_none() && description.is_none() && is_public.is_none() {
        return Err(anyhow!(
            "Nothing to change. Pass at least one of --name, --description \
             (or --description-stdin), --public, or --private."
        ));
    }

    let res = update_project(
        creds,
        project_id,
        &UpdateProjectBody {
            name,
            description,
            is_public,
        },
    )
    .await?;
    let project = res.project;

    println!("\u{2713} Project updated.");
    println!("  id:          {}", project.id);
    println!("  name:        {}", project.name);
    println!(
        "  access:      {}",
        if project.is_public {
            "public"
        } else {
            "private"
        }
    );
    if project.description.is_empty() {
        println!("  description: — (empty)");
    } else {
        println!("  description: {}", project.description);
    }
    Ok(())
}
