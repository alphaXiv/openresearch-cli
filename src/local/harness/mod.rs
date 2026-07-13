//! The harness compatibility layer: one `Harness` trait that every coding-agent
//! integration (Claude Code, Codex, OpenCode, Cursor) implements, plus the
//! single `registry()` that every consumer iterates.
//!
//! A harness can offer up to three capabilities, and no harness is required to
//! offer all of them:
//!
//! * **detection** (`detect`) — is the CLI installed, is the user signed in,
//!   which account/models. Powers `orx up`'s harness picker.
//! * **chat** (`run_turn`) — drive one chat turn by spawning the CLI and
//!   normalizing its native event stream into wire parts. Detection-only or
//!   install-only harnesses leave this at its default (unsupported).
//! * **skill install** (`skill_target` / `skill_shim`) — drop the `orx` skill
//!   shim so the agent auto-discovers the CLI. Cursor offers only this.
//!
//! Adding a fourth harness is one new file with one `impl Harness` and one line
//! in `registry()`; the dispatch, the ID list, the detection sweep, and the
//! skill installer all pick it up with no further edits.

mod claude;
pub(crate) mod codex;
mod cursor;
mod detect;
mod opencode;
mod options;

use std::path::PathBuf;

use async_trait::async_trait;

use crate::error::{anyhow, Result};
use crate::local::chat::{PromptAnswer, ResumeCtx, TurnCtx, WirePrompt};

pub use detect::{HarnessInfo, ModelInfo};
pub use options::{HarnessOptions, PermissionMode};

/// How an answered interactive prompt flows back into the harness. The two axes
/// a harness can live on:
///
/// * **End-turn-and-resume** (Claude Code): a prompt ends the CLI turn, and the
///   answer becomes a *new user message* that continues the native session via
///   `--resume`. These harnesses return [`ResumeAction::SendMessage`] and
///   `ChatHost` spawns a fresh turn with that text + mode.
/// * **Inline over a live protocol** (OpenCode): the turn is still running,
///   paused on a `permission.asked` / `question.asked` over the serve session;
///   the answer is POSTed back to that live process, which unblocks it. These
///   harnesses perform the reply themselves in `resume_from_prompt` (they own
///   the endpoint shape, and reach their live process through the `ResumeCtx`
///   host handle) and return [`ResumeAction::Handled`] — no new turn to spawn.
///
/// Keeping the decision behind the trait is what lets `ChatHost::respond` stay
/// harness-agnostic (mark resolved, busy-check, broadcast idle) while never
/// routing an inline-approval harness through the new-message resume path.
pub enum ResumeAction {
    /// Resume by sending `text` as a new user message under `mode` (Claude).
    SendMessage {
        text: String,
        mode: Option<PermissionMode>,
    },
    /// The harness already delivered the answer to its live process (OpenCode
    /// inline reply). Nothing left for `ChatHost` but to clear `busy`.
    Handled,
    /// No resume — e.g. a denied permission that just closes the card.
    Nothing,
}

/// One coding-agent integration. See the module docs for the capability model.
#[async_trait]
pub trait Harness: Send + Sync {
    /// Canonical, stable id used on the wire and in the store
    /// (e.g. `"claude-code"`). Must be unique across the registry.
    fn id(&self) -> &'static str;

    /// Human-readable name for UI and prompts (e.g. `"Claude Code"`).
    fn name(&self) -> &'static str;

    // --- chat capability ---------------------------------------------------

    /// Whether this harness can drive chat turns. Gates it out of the chat
    /// picker and the create-session allowlist.
    fn supports_chat(&self) -> bool {
        false
    }

    /// Detect install/auth/account/model state for the `orx up` picker.
    /// `None` means this harness isn't a chat backend and shouldn't appear.
    async fn detect(&self) -> Option<HarnessInfo> {
        None
    }

    /// Run one chat turn: spawn the CLI, parse its event stream, push wire
    /// parts onto `ctx`. Default is "not a chat harness".
    async fn run_turn(&self, _ctx: &mut TurnCtx) -> Result<()> {
        Err(anyhow!("{} cannot run chat turns", self.id()))
    }

    /// The permission-mode / reasoning-level vocabulary this harness supports,
    /// for the composer toggles. Default is neither control (the UI hides both).
    fn options(&self) -> HarnessOptions {
        HarnessOptions::none()
    }

    /// Decide how an answered prompt flows back, and (for inline harnesses)
    /// deliver it. See [`ResumeAction`] for the two shapes. This runs *before*
    /// `ChatHost::respond` marks the card resolved, so returning an `Err` (e.g.
    /// an unanswerable selection, or a failed inline delivery) leaves the card
    /// actionable and retryable.
    ///
    /// * End-turn harnesses (Claude) build a [`ResumeAction::SendMessage`] and
    ///   let `ChatHost` spawn the follow-up turn.
    /// * Inline harnesses (OpenCode) POST the reply to their live process here,
    ///   reaching it through the `ctx` host handle + native session id, and
    ///   return [`ResumeAction::Handled`].
    ///
    /// The default is [`ResumeAction::Nothing`] — a harness that never emits
    /// prompts never has one to answer.
    async fn resume_from_prompt(
        &self,
        _ctx: &ResumeCtx,
        _prompt: &WirePrompt,
        _answer: &PromptAnswer,
    ) -> Result<ResumeAction> {
        Ok(ResumeAction::Nothing)
    }

    // --- skill-install capability -----------------------------------------

    /// The agent's config home (`~/.claude`, `~/.codex`, `~/.config/opencode`,
    /// `~/.cursor`). Presence of this dir is how we tell the agent is set up.
    /// `None` if this harness has no installable skill.
    fn config_home(&self) -> Option<PathBuf> {
        None
    }

    /// Where the skill shim file lands. `None` if not installable.
    fn skill_target(&self) -> Option<PathBuf> {
        None
    }

    /// The shim file contents to write at `skill_target`. `None` if not
    /// installable.
    fn skill_shim(&self) -> Option<&'static str> {
        None
    }

    /// True if the agent looks set up on this machine (its config home exists).
    fn is_installed_locally(&self) -> bool {
        self.config_home().map(|h| h.exists()).unwrap_or(false)
    }
}

/// The one registry. Every consumer — chat dispatch, detection sweep, the
/// create-session allowlist, and the skill installer — iterates this.
pub fn registry() -> Vec<Box<dyn Harness>> {
    vec![
        Box::new(claude::ClaudeCode),
        Box::new(codex::Codex),
        Box::new(opencode::OpenCode),
        Box::new(cursor::Cursor),
    ]
}

/// The chat-capable harness with this id, if any (used by chat dispatch).
pub fn chat_harness(id: &str) -> Option<Box<dyn Harness>> {
    registry()
        .into_iter()
        .find(|h| h.id() == id && h.supports_chat())
}

/// True if `id` names a chat-capable harness (create-session allowlist).
pub fn is_chat_harness(id: &str) -> bool {
    registry().iter().any(|h| h.id() == id && h.supports_chat())
}

/// Detect every chat-capable harness, in registry order. This is what the
/// `orx up` dashboard renders in its harness picker.
pub async fn detect_harnesses() -> Vec<HarnessInfo> {
    let harnesses: Vec<Box<dyn Harness>> = registry()
        .into_iter()
        .filter(|h| h.supports_chat())
        .collect();
    let futures = harnesses.iter().map(|h| async {
        // Attach the composer toggle vocabulary alongside detection so the UI
        // gets both in one payload.
        h.detect().await.map(|mut info| {
            info.options = h.options();
            info
        })
    });
    futures::future::join_all(futures)
        .await
        .into_iter()
        .flatten()
        .collect()
}

/// `$XDG_CONFIG_HOME`, or `~/.config` as the fallback. Mirrors `config::config_dir`
/// — and notably stays XDG even on macOS (OpenCode uses `~/.config/opencode`, not
/// `~/Library/Application Support`). Shared by the harnesses keyed off XDG config.
pub(crate) fn xdg_config_home() -> PathBuf {
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

// --- skill shims --------------------------------------------------------------
//
// The shim deliberately carries no operating instructions of its own: it just
// tells the agent to run `orx skill` to load the live guide. The real guidance
// stays in the CLI's SKILL.md and is fetched fresh each session, so the
// installed shim never drifts as that guide changes — which it does, often.

/// Claude Code / OpenCode / Cursor skill (`skills/orx/SKILL.md`). The frontmatter
/// `description` drives auto-discovery and the `/orx` invocation; the body only
/// points the agent at the live guide.
pub(super) const CLAUDE_SKILL: &str = r#"---
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

Follow the auto-research loop from the guide: create the baseline experiment
first when the project is empty, branch variants off it, launch runs within the
user's GPU budget, wait on completions, and analyze each result before deciding
to refill, promote, or stop.

## Prerequisite

The user must be logged in. If any command reports `Not logged in`, ask them to
run `orx login`.
"#;

/// Codex prompt (`~/.codex/prompts/orx.md`), invoked as `/orx`. Plain markdown for
/// broad version compatibility; `$ARGUMENTS` is substituted with whatever the user
/// types after the command (and reads fine as-is if their Codex doesn't expand it).
pub(super) const CODEX_PROMPT: &str = r#"Drive automated ML research on OpenResearch using the `orx` CLI.

Start by running `orx skill` to load the current operating manual — the cardinal
rules, the full command reference, the experiment-tree model, and the
auto-research loop. It changes often, so always read it fresh rather than relying
on memory or a cached copy.

Then carry out the user's research goal, following the auto-research loop from that
guide: create the baseline experiment first when the project is empty, branch
variants off it, launch runs within the GPU budget, wait on completions, and
analyze each result before deciding to refill, promote, or stop.

If any command reports `Not logged in`, ask the user to run `orx login` first.

Research goal:
$ARGUMENTS
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn options_for(id: &str) -> HarnessOptions {
        registry()
            .into_iter()
            .find(|h| h.id() == id)
            .unwrap_or_else(|| panic!("no harness {id}"))
            .options()
    }

    fn mode_ids(o: &HarnessOptions) -> Vec<&str> {
        o.permission_modes.iter().map(|c| c.id).collect()
    }
    fn reasoning_ids(o: &HarnessOptions) -> Vec<&str> {
        o.reasoning_levels.iter().map(|c| c.id).collect()
    }

    /// Pin each harness's advertised composer vocabulary — this is the wire
    /// contract the UI renders, and the whole point of the parity work. All ids
    /// must be the neutralized (harness-agnostic) permission-mode spellings.
    #[test]
    fn advertised_options_per_harness() {
        // Claude: only Auto + Bypass. `ask`/`accept-edits` aren't grantable
        // headless, and `plan` fought the orx workflow — all three dropped.
        let claude = options_for("claude-code");
        assert_eq!(mode_ids(&claude), ["auto", "bypass"]);
        assert_eq!(claude.default_permission_mode, Some("auto"));
        assert_eq!(
            reasoning_ids(&claude),
            ["low", "medium", "high", "xhigh", "max"]
        );

        // Codex: Auto + Bypass (matches Claude — `plan`/read-only dropped for the
        // same reason; `codex exec` has no real plan mode). Codex reasoning tiers.
        let codex = options_for("codex");
        assert_eq!(mode_ids(&codex), ["auto", "bypass"]);
        assert_eq!(codex.default_permission_mode, Some("auto"));
        assert_eq!(reasoning_ids(&codex), ["low", "medium", "high", "xhigh"]);

        // OpenCode: Plan (the native plan agent) + Auto (its permissive default)
        // + Bypass. No `ask` — opencode's default rarely prompts, so a dedicated
        // ask mode would be hollow. No reasoning axis.
        let opencode = options_for("opencode");
        assert_eq!(mode_ids(&opencode), ["plan", "auto", "bypass"]);
        assert_eq!(opencode.default_permission_mode, Some("auto"));
        assert!(opencode.reasoning_levels.is_empty());
    }

    /// Every advertised permission-mode id must round-trip through
    /// `PermissionMode::from_id` — i.e. a harness never advertises an id the
    /// backend can't parse back when the session sends it.
    #[test]
    fn advertised_permission_ids_all_parse() {
        for h in registry() {
            for choice in h.options().permission_modes {
                assert!(
                    PermissionMode::from_id(choice.id).is_some(),
                    "{} advertises unparseable mode {:?}",
                    h.id(),
                    choice.id
                );
            }
        }
    }
}
