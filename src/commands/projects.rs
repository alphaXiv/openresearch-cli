//! The `projects` command. Lists project names, grouped by organization.

use crate::client::{list_orgs, list_projects};
use crate::error::require_credentials;
use crate::error::Result;

/// Lists project names, grouped by organization.
pub async fn run(args: crate::ProjectsArgs) -> Result<()> {
    let creds = require_credentials().await;
    let orgs = list_orgs(&creds).await?.orgs;

    if orgs.is_empty() {
        if args.json {
            println!("[]");
        } else {
            println!("No organizations found for this account.");
        }
        return Ok(());
    }

    // Machine-readable form: a flat array of projects, each tagged with its org
    // and paperId — what the publish-to-alphaXiv sweep consumes.
    if args.json {
        let mut out: Vec<serde_json::Value> = Vec::new();
        for org in &orgs {
            let projects = list_projects(&creds, &org.id).await?.projects;
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

    for org in &orgs {
        let projects = list_projects(&creds, &org.id).await?.projects;
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
