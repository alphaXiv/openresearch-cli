//! The `explore` command — browse the public project directory.
//!
//! Lists every project flagged public (`GET /projects/public`), which is
//! viewable by anyone. Pull one apart next with `orx project view <projectId>`,
//! then `orx report show` / `orx experiments` / `orx runs` on its id.

use crate::client::list_public_projects;
use crate::error::{require_credentials, Result};
use crate::output::print_table;

pub async fn run(args: crate::ExploreArgs) -> Result<()> {
    let creds = require_credentials().await;
    let projects = list_public_projects(&creds).await?.projects;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&projects)?);
        return Ok(());
    }

    if projects.is_empty() {
        println!("No public projects yet.");
        return Ok(());
    }

    // Id first so it's easy to copy into `orx project view/experiments/runs <id>`.
    let rows: Vec<Vec<String>> = projects
        .iter()
        .map(|p| {
            let repo = if p.github_owner.is_empty() {
                String::new()
            } else {
                format!("{}/{}", p.github_owner, p.github_repo)
            };
            vec![p.id.clone(), p.name.clone(), repo]
        })
        .collect();
    print_table(&["ID", "NAME", "REPO"], &rows);
    eprintln!("\nDig in with: orx project view <projectId>");
    Ok(())
}
