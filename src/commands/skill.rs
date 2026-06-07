use crate::config;
use crate::error::{require_credentials, Result};

// Bundled top-level overview, shipped with the CLI so `orx skill` works without
// a round-trip. Deeper references are fetched live from the API. Embedded at
// compile time from the repo-root SKILL.md.
const SKILL_MD: &str = include_str!("../../SKILL.md");

pub async fn run(args: crate::SkillArgs) -> Result<()> {
    // With a path: fetch the canonical doc from the API (same docs the assistant
    // reads), so the schema never drifts from a hand-maintained copy.
    if let Some(path) = args.path {
        let creds = require_credentials().await;
        let content = crate::client::read_skill(&creds, &path).await?;
        println!("{}", content.content);
        return Ok(());
    }

    // No path: print the bundled overview, then list fetchable skills (best
    // effort — skip the index if we can't reach the API).
    println!("{}", SKILL_MD);

    let creds = match config::load_credentials().await? {
        Some(c) => c,
        None => return Ok(()),
    };

    // API unreachable — the bundled overview is enough on its own, so ignore Err.
    if let Ok(list) = crate::client::list_skills(&creds).await {
        if !list.skills.is_empty() {
            println!("\nFetchable skills (orx skill <path>):");
            for s in &list.skills {
                println!("  {}", s.path);
            }
        }
    }

    Ok(())
}
