use std::path::{Path, PathBuf};

use crate::config;
use crate::error::{anyhow, require_credentials, Result};
use crate::{SkillAgent, SkillCommand, SkillInstallArgs};

// Bundled top-level overview, shipped with the CLI so `orx skill` works without
// a round-trip. Deeper references are fetched live from the API. Embedded at
// compile time from the repo-root SKILL.md.
const SKILL_MD: &str = include_str!("../../SKILL.md");

// Per the Agent Skills convention, the directory name is the skill's identity
// and must match the `name:` in SKILL.md's frontmatter.
const SKILL_DIR_NAME: &str = "openresearch-cli";

// Claude Code reads its own skills tree; Codex, Cursor, and OpenCode all read
// the cross-agent standard location (agentskills.io).
const CLAUDE_SKILLS: &str = ".claude/skills";
const AGENTS_SKILLS: &str = ".agents/skills";

pub async fn run(args: crate::SkillArgs) -> Result<()> {
    if let Some(SkillCommand::Install(install)) = args.command {
        return install_skill(install).await;
    }

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

/// Skill directories (relative to `$HOME`) for an agent selection.
fn install_dirs(agent: SkillAgent, all: bool) -> Vec<&'static str> {
    if all {
        return vec![CLAUDE_SKILLS, AGENTS_SKILLS];
    }
    match agent {
        SkillAgent::Claude => vec![CLAUDE_SKILLS],
        SkillAgent::Codex | SkillAgent::Cursor | SkillAgent::Opencode => vec![AGENTS_SKILLS],
    }
}

/// Writes the bundled SKILL.md under `home/rel/openresearch-cli/`, creating
/// directories as needed. Returns the file path and whether it was (re)written
/// — false means an identical copy was already there, so re-runs are no-ops.
async fn install_into(home: &Path, rel: &str) -> Result<(PathBuf, bool)> {
    let dir = home.join(rel).join(SKILL_DIR_NAME);
    let path = dir.join("SKILL.md");
    if matches!(tokio::fs::read_to_string(&path).await, Ok(existing) if existing == SKILL_MD) {
        return Ok((path, false));
    }
    tokio::fs::create_dir_all(&dir).await?;
    tokio::fs::write(&path, SKILL_MD).await?;
    Ok((path, true))
}

/// `orx skill install` — no login or network needed; the skill is embedded in
/// the binary, and placing the folder is the entire installation.
async fn install_skill(args: SkillInstallArgs) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not determine home directory"))?;
    let rels = install_dirs(args.agent, args.all);
    for rel in &rels {
        let (path, wrote) = install_into(&home, rel).await?;
        if wrote {
            println!("✓ wrote {}", path.display());
        } else {
            println!("✓ already up to date: {}", path.display());
        }
    }
    if rels.contains(&AGENTS_SKILLS) {
        println!("~/{AGENTS_SKILLS} is read by Codex, Cursor, and OpenCode.");
    }
    println!("New agent sessions discover the skill automatically; restart any open session.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::ValueEnum;

    #[test]
    fn install_dirs_mapping() {
        assert_eq!(install_dirs(SkillAgent::Claude, false), vec![CLAUDE_SKILLS]);
        for agent in [SkillAgent::Codex, SkillAgent::Cursor, SkillAgent::Opencode] {
            assert_eq!(install_dirs(agent, false), vec![AGENTS_SKILLS]);
        }
        assert_eq!(install_dirs(SkillAgent::Claude, true), vec![CLAUDE_SKILLS, AGENTS_SKILLS]);
    }

    #[test]
    fn desktop_variants_are_aliases() {
        let parse = |s| SkillAgent::from_str(s, false).unwrap();
        assert_eq!(parse("claude-desktop"), SkillAgent::Claude);
        assert_eq!(parse("codex-desktop"), SkillAgent::Codex);
        assert!(SkillAgent::from_str("nope", false).is_err());
    }

    #[tokio::test]
    async fn install_writes_then_noops() {
        let home = tempfile::tempdir().unwrap();
        let (path, wrote) = install_into(home.path(), CLAUDE_SKILLS).await.unwrap();
        assert!(wrote);
        assert_eq!(
            path,
            home.path().join(CLAUDE_SKILLS).join(SKILL_DIR_NAME).join("SKILL.md")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SKILL_MD);

        // Identical content already present — second run reports up to date.
        let (_, wrote) = install_into(home.path(), CLAUDE_SKILLS).await.unwrap();
        assert!(!wrote);

        // A stale copy (e.g. from an older binary) gets overwritten.
        std::fs::write(&path, "old").unwrap();
        let (_, wrote) = install_into(home.path(), CLAUDE_SKILLS).await.unwrap();
        assert!(wrote);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SKILL_MD);
    }
}
