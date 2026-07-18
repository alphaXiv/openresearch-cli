//! `orx install-skills` — drop a thin "skill" shim into the local coding agents
//! (Claude Code, Codex, OpenCode, Cursor) so they auto-discover how to drive
//! `orx`.
//!
//! The shim, its target path, and each agent's config home all live on the
//! `Harness` trait (`src/local/harness/`), so this command is just the CLI
//! surface over the registry: it selects which installable harnesses to write
//! and reports what it wrote. Adding a fourth installable agent needs no change
//! here.
//!
//! Detection is by config-home presence (`Harness::is_installed_locally`).
//! After `orx login`, the install is *offered* interactively (see
//! `offer_install_after_login`) — what gets written where is listed up front,
//! and nothing is written without a yes.

use std::path::{Path, PathBuf};

use tokio::fs;

use crate::error::{anyhow, Result};
use crate::local::harness::{registry, Harness};

/// Every harness that ships an installable skill shim, in registry order.
fn installable() -> Vec<Box<dyn Harness>> {
    registry()
        .into_iter()
        .filter(|h| h.skill_target().is_some())
        .collect()
}

/// Write (or overwrite) one harness's shim files, creating parent dirs as
/// needed. Overwriting is intentional: re-running keeps the shim current. A
/// harness may write more than one file (Codex: the native skill + a legacy
/// prompt); the primary target is returned first, extras after.
async fn write_shim(harness: &dyn Harness) -> Result<Vec<PathBuf>> {
    let primary = harness
        .skill_target()
        .ok_or_else(|| anyhow!("{} has no installable skill", harness.name()))?;
    let shim = harness
        .skill_shim()
        .ok_or_else(|| anyhow!("{} has no skill shim", harness.name()))?;
    let mut written = Vec::new();
    for (path, contents) in std::iter::once((primary, shim)).chain(harness.extra_skill_targets()) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(&path, contents).await?;
        written.push(path);
    }
    Ok(written)
}

/// Non-interactive OpenCode install, for the `orx up` agent bootstrap: the
/// spawned opencode discovers `orx` via its skill tool.
pub(crate) async fn install_opencode_shim() -> Result<Vec<PathBuf>> {
    let harness = registry()
        .into_iter()
        .find(|h| h.id() == "opencode")
        .ok_or_else(|| anyhow!("opencode harness not registered"))?;
    write_shim(harness.as_ref()).await
}

pub async fn run(args: crate::InstallSkillsArgs) -> Result<()> {
    let all = installable();
    let targets: Vec<&dyn Harness> = match args.agent.as_deref() {
        Some("all") | Some("both") => all.iter().map(Box::as_ref).collect(),
        Some(name) => {
            let selected: Vec<&dyn Harness> = all
                .iter()
                .map(Box::as_ref)
                .filter(|h| matches_agent(*h, name))
                .collect();
            if selected.is_empty() {
                return Err(anyhow!(
                    "unknown agent '{name}' (expected: claude, codex, opencode, cursor, or all)"
                ));
            }
            selected
        }
        None => {
            // Auto-detect agents already set up. If none are, install to all
            // anyway so the shim is waiting whenever the user installs one.
            let present: Vec<&dyn Harness> = all
                .iter()
                .map(Box::as_ref)
                .filter(|h| h.is_installed_locally())
                .collect();
            if present.is_empty() {
                all.iter().map(Box::as_ref).collect()
            } else {
                present
            }
        }
    };

    for harness in targets {
        for path in write_shim(harness).await? {
            println!(
                "\u{2713} Installed {} skill \u{2192} {}",
                harness.name(),
                path.display()
            );
        }
        if args.full {
            for path in write_full_skills(harness).await? {
                println!(
                    "\u{2713} Installed {} skill \u{2192} {}",
                    harness.name(),
                    path.display()
                );
            }
        }
    }
    println!("\nYour agent will auto-load it, or you can invoke it with /orx.");
    Ok(())
}

/// The agent's **global** skills directory — where a native `SKILL.md` skill
/// dir per module goes. Derived from the primary shim target
/// (`<skills-dir>/orx/SKILL.md`), so it tracks each harness's real layout
/// (`~/.claude/skills`, `~/.agents/skills` for Codex, `$XDG/opencode/skills`,
/// `~/.cursor/skills`).
fn global_skills_dir(harness: &dyn Harness) -> Option<PathBuf> {
    Some(harness.skill_target()?.parent()?.parent()?.to_path_buf())
}

/// Write the full set of modular `orx` skills into the harness's global skills
/// dir (`--full`). Overwrites in place; returns the `SKILL.md` paths written.
async fn write_full_skills(harness: &dyn Harness) -> Result<Vec<PathBuf>> {
    use crate::local::agent_skills::{skills, SkillSet};

    let base = global_skills_dir(harness)
        .ok_or_else(|| anyhow!("{} has no global skills dir", harness.name()))?;
    let mut written = Vec::new();
    for skill in skills(SkillSet::Full) {
        let dir = base.join(skill.name);
        fs::create_dir_all(&dir).await?;
        let path = dir.join("SKILL.md");
        fs::write(&path, skill.content).await?;
        written.push(path);
    }
    Ok(written)
}

/// The CLI `--agent` alias for a harness. The chat id (`claude-code`) differs
/// from the historical CLI word (`claude`), so accept both.
fn matches_agent(harness: &dyn Harness, name: &str) -> bool {
    match harness.id() {
        "claude-code" => name == "claude" || name == "claude-code",
        id => id == name,
    }
}

/// A path with the home directory shortened to `~`, for prompt readability.
fn tilde(path: &Path) -> String {
    let s = path.to_string_lossy();
    match dirs::home_dir() {
        Some(home) => match path.strip_prefix(&home) {
            Ok(rel) => format!("~/{}", rel.display()),
            Err(_) => s.into_owned(),
        },
        None => s.into_owned(),
    }
}

/// The consent prompt's body: one line per detected agent target file (a
/// harness with more than one — Codex — gets one line per file).
fn describe_targets(harnesses: &[&dyn Harness]) -> String {
    let width = harnesses.iter().map(|h| h.name().len()).max().unwrap_or(0);
    harnesses
        .iter()
        .flat_map(|h| {
            let primary = h.skill_target();
            let extras = h.extra_skill_targets();
            primary
                .into_iter()
                .chain(extras.into_iter().map(|(p, _)| p))
                .map(move |target| format!("  {:<width$} \u{2192} {}", h.name(), tilde(&target)))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Offer (never force) the skill install after `orx login`. Transparent and
/// consensual: lists exactly what would be written where before asking, writes
/// nothing without a yes, and never fails the login over it. Skips entirely
/// when stdin/stderr isn't a terminal (CI, agents, pipes) or no agent is set up
/// on this machine.
pub async fn offer_install_after_login() {
    use std::io::IsTerminal;

    let all = installable();
    let present: Vec<&dyn Harness> = all
        .iter()
        .map(Box::as_ref)
        .filter(|h| h.is_installed_locally())
        .collect();
    if present.is_empty() || !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return;
    }

    println!(
        "\norx ships one agent skill: a small file teaching your coding agent to run\n\
         `orx skill` for the live guide. It can be installed now ({} file{}) for the\n\
         agent{} detected on this machine:",
        present.len(),
        if present.len() == 1 { "" } else { "s" },
        if present.len() == 1 { "" } else { "s" },
    );
    println!("{}", describe_targets(&present));
    print!("Install? [Y/n] ");
    let _ = std::io::Write::flush(&mut std::io::stdout());

    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return;
    }
    if matches!(answer.trim().to_lowercase().as_str(), "" | "y" | "yes") {
        for harness in present {
            if let Ok(paths) = write_shim(harness).await {
                for path in paths {
                    println!(
                        "\u{2713} Installed {} skill \u{2192} {}",
                        harness.name(),
                        tilde(&path)
                    );
                }
            }
        }
    } else {
        println!("Skipped. You can install any time with: orx install-skills");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_targets_lists_name_and_path_per_agent() {
        let all = installable();
        let claude = all
            .iter()
            .find(|h| h.id() == "claude-code")
            .unwrap()
            .as_ref();
        let cursor = all.iter().find(|h| h.id() == "cursor").unwrap().as_ref();
        let body = describe_targets(&[claude, cursor]);
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("Claude Code"));
        assert!(lines[0].contains(".claude/skills/orx/SKILL.md"));
        assert!(lines[1].contains("Cursor"));
        assert!(lines[1].contains(".cursor/skills/orx/SKILL.md"));
    }

    #[test]
    fn tilde_shortens_home() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(tilde(&home.join(".claude")), "~/.claude");
        assert_eq!(tilde(std::path::Path::new("/etc/hosts")), "/etc/hosts");
    }

    #[test]
    fn every_installable_harness_has_a_shim() {
        for h in installable() {
            assert!(h.skill_shim().is_some(), "{} missing shim", h.id());
            assert!(h.config_home().is_some(), "{} missing config home", h.id());
        }
    }

    /// Codex migrated to a native SKILL.md (`~/.agents/skills/orx/SKILL.md`) but
    /// still writes the legacy `/orx` prompt alongside — two targets, listed in
    /// the consent prompt.
    #[test]
    fn codex_writes_native_skill_and_legacy_prompt() {
        let all = installable();
        let codex = all.iter().find(|h| h.id() == "codex").unwrap().as_ref();

        let primary = codex.skill_target().unwrap();
        assert!(
            primary.ends_with(".agents/skills/orx/SKILL.md"),
            "primary target is the native skill, got {}",
            primary.display()
        );
        let extras = codex.extra_skill_targets();
        assert_eq!(extras.len(), 1, "one legacy prompt");
        assert!(
            extras[0].0.ends_with(".codex/prompts/orx.md"),
            "legacy prompt path, got {}",
            extras[0].0.display()
        );

        let body = describe_targets(&[codex]);
        assert_eq!(body.lines().count(), 2, "both codex targets listed");
    }

    /// The `--full` global skills dir is the parent of the harness's skill dir
    /// (`~/.claude/skills`, `~/.agents/skills` for codex, `$XDG/opencode/skills`).
    #[test]
    fn global_skills_dir_is_the_skills_parent() {
        let all = installable();
        for h in all.iter().map(Box::as_ref) {
            let dir = global_skills_dir(h).unwrap();
            // The primary shim target sits at <dir>/orx/SKILL.md.
            assert_eq!(
                h.skill_target().unwrap(),
                dir.join("orx").join("SKILL.md"),
                "{} global skills dir mismatch",
                h.id()
            );
            assert!(
                dir.ends_with("skills"),
                "{} global skills dir should end in `skills`, got {}",
                h.id(),
                dir.display()
            );
        }
    }
}
