use crate::config;
use crate::error::{require_credentials, Result};
use crate::local::agent_skills::{self, SkillSet};

// Bundled top-level overview, shipped with the CLI so `orx skill` works without
// a round-trip. Deeper references are fetched live from the API. Embedded at
// compile time from the repo-root SKILL.md.
const SKILL_MD: &str = include_str!("../../SKILL.md");

pub async fn run(args: crate::SkillArgs) -> Result<()> {
    if let Some(path) = args.path {
        // First: a bundled module (with or without the `orx-` prefix). These
        // ship in the binary, so they resolve offline and never drift.
        if let Some(skill) = agent_skills::find(&path) {
            println!("{}", skill.content.trim_end());
            return Ok(());
        }
        // Otherwise fetch the canonical doc from the API (same docs the assistant
        // reads), so the schema never drifts from a hand-maintained copy.
        let creds = require_credentials().await;
        let content = crate::client::read_skill(&creds, &path).await?;
        println!("{}", content.content);
        return Ok(());
    }

    // No path: print the bundled overview, then the bundled module index, then
    // list API-fetchable deep references (best effort — skip if unreachable).
    println!("{}", SKILL_MD);

    println!("\nBundled modules (orx skill <name>):");
    for s in agent_skills::skills(SkillSet::Full) {
        println!("  {:<20} {}", s.name, s.description);
    }

    let creds = match config::load_credentials().await? {
        Some(c) => c,
        None => return Ok(()),
    };

    // API unreachable — the bundled overview + modules are enough, so ignore Err.
    if let Ok(list) = crate::client::list_skills(&creds).await {
        if !list.skills.is_empty() {
            println!("\nFetchable references (orx skill <path>):");
            for s in &list.skills {
                println!("  {}", s.path);
            }
        }
    }

    Ok(())
}
