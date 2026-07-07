//! `orx install-skills` — drop a thin "skill" shim into the local coding agents
//! (Claude Code, Codex) so they auto-discover how to drive `orx`.
//!
//! The shim deliberately carries no operating instructions of its own: it just
//! tells the agent to run `orx skill` to load the live guide. The real guidance
//! (cardinal rules, command reference, the auto-research loop) stays in the CLI's
//! SKILL.md and is fetched fresh each session, so the installed shim never drifts
//! as that guide changes — which it does, often.
//!
//! Detection is by config-home presence: `~/.claude` means Claude Code is set up,
//! `~/.codex` means Codex is, `~/.config/opencode` means OpenCode is, `~/.cursor`
//! means Cursor is. After `orx login`, the install is *offered* interactively
//! (see `offer_install_after_login`) — what gets written where is listed up
//! front, and nothing is written without a yes.

use std::path::PathBuf;

use tokio::fs;

use crate::error::{anyhow, Result};

/// `$XDG_CONFIG_HOME`, or `~/.config` as the fallback. Mirrors `config::config_dir`
/// — and notably stays XDG even on macOS (OpenCode uses `~/.config/opencode`, not
/// `~/Library/Application Support`).
fn xdg_config_home() -> PathBuf {
    // Ignore an unset *or* empty value — a set-but-empty XDG_CONFIG_HOME would
    // otherwise resolve to a relative `opencode/` path under the cwd.
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
        })
}

/// A coding agent we can install the shim into.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
    OpenCode,
    Cursor,
}

const ALL: [Agent; 4] = [Agent::Claude, Agent::Codex, Agent::OpenCode, Agent::Cursor];

impl Agent {
    fn label(self) -> &'static str {
        match self {
            Agent::Claude => "Claude Code",
            Agent::Codex => "Codex",
            Agent::OpenCode => "OpenCode",
            Agent::Cursor => "Cursor",
        }
    }

    /// The agent's config home (`~/.claude`, `~/.codex`, `~/.config/opencode`,
    /// `~/.cursor`). `None` only if we can't resolve the user's home directory
    /// at all.
    fn home(self) -> Option<PathBuf> {
        Some(match self {
            Agent::Claude => dirs::home_dir()?.join(".claude"),
            Agent::Codex => dirs::home_dir()?.join(".codex"),
            Agent::OpenCode => xdg_config_home().join("opencode"),
            Agent::Cursor => dirs::home_dir()?.join(".cursor"),
        })
    }

    /// Where the shim file lands. Claude, OpenCode, and Cursor discover skills
    /// under `skills/<name>/SKILL.md` (OpenCode and Cursor also scan
    /// `~/.claude/skills`, but we write their native paths so users without
    /// Claude are covered); Codex exposes `prompts/<name>.md` as `/<name>`.
    /// Claude and Codex resolve to `/orx`; OpenCode auto-loads the skill via its
    /// `skill` tool (no slash command).
    fn target(self) -> Option<PathBuf> {
        let home = self.home()?;
        Some(match self {
            Agent::Claude | Agent::OpenCode | Agent::Cursor => {
                home.join("skills").join("orx").join("SKILL.md")
            }
            Agent::Codex => home.join("prompts").join("orx.md"),
        })
    }

    fn shim(self) -> &'static str {
        match self {
            // OpenCode and Cursor read the same SKILL.md format as Claude Code
            // (name + description frontmatter), so they share the shim.
            Agent::Claude | Agent::OpenCode | Agent::Cursor => CLAUDE_SKILL,
            Agent::Codex => CODEX_PROMPT,
        }
    }

    /// True if the agent looks set up on this machine (its config home exists).
    fn is_present(self) -> bool {
        self.home().map(|h| h.exists()).unwrap_or(false)
    }
}

/// Write (or overwrite) one agent's shim, creating parent dirs as needed.
/// Overwriting is intentional: re-running keeps the shim current.
async fn write_shim(agent: Agent) -> Result<PathBuf> {
    let path = agent
        .target()
        .ok_or_else(|| anyhow!("could not resolve your home directory"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(&path, agent.shim()).await?;
    Ok(path)
}

/// Non-interactive OpenCode install, for the `orx up` agent bootstrap: the
/// spawned opencode discovers `orx` via its skill tool.
pub(crate) async fn install_opencode_shim() -> Result<PathBuf> {
    write_shim(Agent::OpenCode).await
}

pub async fn run(args: crate::InstallSkillsArgs) -> Result<()> {
    let targets = match args.agent.as_deref() {
        Some("claude") => vec![Agent::Claude],
        Some("codex") => vec![Agent::Codex],
        Some("opencode") => vec![Agent::OpenCode],
        Some("cursor") => vec![Agent::Cursor],
        Some("all") | Some("both") => ALL.to_vec(),
        Some(other) => {
            return Err(anyhow!(
                "unknown agent '{other}' (expected: claude, codex, opencode, cursor, or all)"
            ))
        }
        None => {
            // Auto-detect agents already set up. If none are, install to all
            // anyway so the shim is waiting whenever the user installs one.
            let present: Vec<Agent> = ALL.into_iter().filter(|a| a.is_present()).collect();
            if present.is_empty() {
                ALL.to_vec()
            } else {
                present
            }
        }
    };

    for agent in targets {
        let path = write_shim(agent).await?;
        println!(
            "\u{2713} Installed {} skill \u{2192} {}",
            agent.label(),
            path.display()
        );
    }
    println!("\nYour agent will auto-load it, or you can invoke it with /orx.");
    Ok(())
}

/// A path with the home directory shortened to `~`, for prompt readability.
fn tilde(path: &std::path::Path) -> String {
    let s = path.to_string_lossy();
    match dirs::home_dir() {
        Some(home) => match path.strip_prefix(&home) {
            Ok(rel) => format!("~/{}", rel.display()),
            Err(_) => s.into_owned(),
        },
        None => s.into_owned(),
    }
}

/// The consent prompt's body: one line per detected agent, name → target file.
fn describe_targets(agents: &[Agent]) -> String {
    let width = agents.iter().map(|a| a.label().len()).max().unwrap_or(0);
    agents
        .iter()
        .filter_map(|a| {
            let target = a.target()?;
            Some(format!(
                "  {:<width$} \u{2192} {}",
                a.label(),
                tilde(&target)
            ))
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

    let present: Vec<Agent> = ALL.into_iter().filter(|a| a.is_present()).collect();
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
        for agent in present {
            if let Ok(path) = write_shim(agent).await {
                println!(
                    "\u{2713} Installed {} skill \u{2192} {}",
                    agent.label(),
                    tilde(&path)
                );
            }
        }
    } else {
        println!("Skipped. You can install any time with: orx install-skills");
    }
}

/// Claude Code skill (`~/.claude/skills/orx/SKILL.md`). The frontmatter
/// `description` is what drives auto-discovery and the `/orx` invocation; the body
/// only points the agent at the live guide.
const CLAUDE_SKILL: &str = r#"---
name: orx
description: Drive automated ML research on OpenResearch with the `orx` CLI — create experiments, launch and monitor runs on GPU compute, analyze results and logs, query the evidence DB, and search literature. Use whenever the user wants to understand, explain, explore, or work on an OpenResearch project, run experiments, do auto-research, or mentions orx or OpenResearch.
---

# OpenResearch (`orx`)

You drive OpenResearch through the `orx` command-line tool. The authoritative
operating manual lives inside the CLI and changes often, so **load it fresh at the
start of every session** instead of relying on this file or prior memory.

## 1. Load the live guide

```bash
orx skill
```

This prints the current manual: the cardinal rules, the full command reference,
the experiment-tree model, and the auto-research loop. Read it before taking any
action. For a deeper reference on a specific area, run `orx skill <path>` using
the paths listed at the end of that output.

## 2. Carry out the user's research goal

Follow the auto-research loop from the guide: branch experiments off the baseline,
launch runs within the user's GPU budget, wait on completions, and analyze each
result before deciding to refill, promote, or stop.

## Prerequisite

The user must be logged in. If any command reports `Not logged in`, ask them to
run `orx login`.
"#;

/// Codex prompt (`~/.codex/prompts/orx.md`), invoked as `/orx`. Plain markdown for
/// broad version compatibility; `$ARGUMENTS` is substituted with whatever the user
/// types after the command (and reads fine as-is if their Codex doesn't expand it).
const CODEX_PROMPT: &str = r#"Drive automated ML research on OpenResearch using the `orx` CLI.

Start by running `orx skill` to load the current operating manual — the cardinal
rules, the full command reference, the experiment-tree model, and the
auto-research loop. It changes often, so always read it fresh rather than relying
on memory or a cached copy.

Then carry out the user's research goal, following the auto-research loop from that
guide: branch experiments off the baseline, launch runs within the GPU budget,
wait on completions, and analyze each result before deciding to refill, promote,
or stop.

If any command reports `Not logged in`, ask the user to run `orx login` first.

Research goal:
$ARGUMENTS
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_targets_lists_name_and_path_per_agent() {
        let body = describe_targets(&[Agent::Claude, Agent::Cursor]);
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
}
