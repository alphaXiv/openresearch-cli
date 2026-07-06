//! The `projects` command. Lists project names, grouped by organization,
//! plus any local (orx up) projects — those need no login.

use crate::client::{list_orgs, list_projects};
use crate::error::Result;
use crate::store::Store;

/// Lists project names, grouped by organization.
pub async fn run(args: crate::ProjectsArgs) -> Result<()> {
    let store = Store::open()?;
    let local = store.list_local_projects()?;

    // Credentials gate only the server listing: a local-only user (never
    // logged in) still sees their local projects.
    let creds = match crate::config::load_credentials().await {
        Ok(Some(c)) => Some(c),
        _ => None,
    };
    if creds.is_none() && local.is_empty() {
        eprintln!("Not logged in. Run `orx login` first.");
        std::process::exit(1);
    }

    let orgs = match &creds {
        Some(creds) => list_orgs(creds).await?.orgs,
        None => Vec::new(),
    };

    if orgs.is_empty() && local.is_empty() {
        if args.json {
            println!("[]");
        } else {
            println!("No organizations found for this account.");
        }
        return Ok(());
    }

    // Machine-readable form: a flat array of projects, each tagged with its org
    // and paperId — what the publish-to-alphaXiv sweep consumes. Local projects
    // ride along tagged `"local": true`.
    if args.json {
        let mut out: Vec<serde_json::Value> = Vec::new();
        for p in &local {
            out.push(serde_json::json!({
                "id": p.id,
                "name": p.name,
                "paperId": serde_json::Value::Null,
                "repo": format!("{}/{}", p.github_owner, p.github_repo),
                "archived": false,
                "orgId": serde_json::Value::Null,
                "orgName": "Local (orx up)",
                "local": true,
            }));
        }
        let creds = creds.as_ref();
        for org in &orgs {
            let creds = creds.expect("orgs imply credentials");
            let projects = list_projects(creds, &org.id).await?.projects;
            for p in projects.iter().filter(|p| args.all || !p.archived) {
                let repo = if p.github_owner.is_empty() {
                    None
                } else {
                    Some(format!("{}/{}", p.github_owner, p.github_repo))
                };
                out.push(serde_json::json!({
                    "id": p.id,
                    "name": p.name,
                    "paperId": p.paper_id,
                    "repo": repo,
                    "archived": p.archived,
                    "orgId": org.id,
                    "orgName": org.name,
                }));
            }
        }
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    // Local section first, clearly marked — these ids work with the local-mode
    // command surface (create-experiment, exp, runs, logs), not the api.
    if !local.is_empty() {
        println!("\nLocal (orx up)");
        let id_width = local
            .iter()
            .map(|p| p.id.chars().count())
            .max()
            .unwrap_or(0);
        for p in &local {
            let pad = id_width.saturating_sub(p.id.chars().count());
            println!(
                "  {}{}  {} (local)  ({}/{})",
                p.id,
                " ".repeat(pad),
                p.name,
                p.github_owner,
                p.github_repo
            );
        }
    }

    for org in &orgs {
        let creds = creds.as_ref().expect("orgs imply credentials");
        let projects = list_projects(creds, &org.id).await?.projects;
        let visible: Vec<_> = if args.all {
            projects.iter().collect()
        } else {
            projects.iter().filter(|p| !p.archived).collect()
        };

        // Org id alongside the name — it's what `orx create-project` takes.
        println!("\n{}  (org: {})", org.name, org.id);
        if visible.is_empty() {
            println!("  (no projects)");
            continue;
        }
        // Id first (fixed-width) so names line up and ids are easy to copy into
        // `orx experiments/runs/query <projectId>`.
        let id_width = visible
            .iter()
            .map(|p| p.id.chars().count())
            .max()
            .unwrap_or(0);
        for project in &visible {
            let tag = if project.archived { " (archived)" } else { "" };
            let pad = id_width.saturating_sub(project.id.chars().count());
            let repo = if project.github_owner.is_empty() {
                String::new()
            } else {
                format!("  ({}/{})", project.github_owner, project.github_repo)
            };
            println!(
                "  {}{}  {}{}{}",
                project.id,
                " ".repeat(pad),
                project.name,
                tag,
                repo
            );
        }
    }

    Ok(())
}
