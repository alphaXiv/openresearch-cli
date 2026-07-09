//! Cursor harness — install-only. Cursor scans `~/.cursor/skills/<name>/SKILL.md`
//! in the same frontmatter format as Claude Code, so it can auto-discover the
//! `orx` skill; it has no headless chat surface `orx up` can drive, so it
//! offers no `detect`/`run_turn` (the trait defaults handle that).

use std::path::PathBuf;

use async_trait::async_trait;

use super::Harness;

pub struct Cursor;

#[async_trait]
impl Harness for Cursor {
    fn id(&self) -> &'static str {
        "cursor"
    }

    fn name(&self) -> &'static str {
        "Cursor"
    }

    fn config_home(&self) -> Option<PathBuf> {
        Some(dirs::home_dir()?.join(".cursor"))
    }

    fn skill_target(&self) -> Option<PathBuf> {
        Some(
            self.config_home()?
                .join("skills")
                .join("orx")
                .join("SKILL.md"),
        )
    }

    fn skill_shim(&self) -> Option<&'static str> {
        // Same SKILL.md format as Claude Code.
        Some(super::CLAUDE_SKILL)
    }
}
