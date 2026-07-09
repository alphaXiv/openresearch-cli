//! Cross-harness turn options — the permission mode and reasoning level a chat
//! session runs under. These are the vocabulary the UI toggles speak; each
//! harness advertises which values it supports (`options()`) and maps the
//! chosen value onto its own CLI (in its `run_turn`).
//!
//! The two axes are modeled differently on purpose:
//!
//! * Permission mode is a *shared* enum — the concept (ask / accept-edits /
//!   plan / auto / bypass) is common enough to name once. Its wire ids happen
//!   to equal Claude Code's `--permission-mode` values today (so Claude's map
//!   is a no-op); a second harness maps the nearest equivalent, and if that
//!   proves lossy the ids can be neutralized (they're the wire/store contract,
//!   so that's a migration — do it when Codex approvals land, not speculatively).
//! * Reasoning level is deliberately NOT shared — Claude's tiers (`low`…`max`,
//!   via `--effort`) and a future Codex's (`low`/`medium`/`high`) genuinely
//!   differ — so each harness owns its own `OptionChoice` list and interprets
//!   the chosen id itself.
//!
//! A harness that doesn't support an axis lists nothing for it, and the composer
//! hides that control.

use serde::{Deserialize, Serialize};

/// How much the harness should defer to the user before acting. The wire ids
/// currently equal Claude Code's `--permission-mode` values (`default`,
/// `acceptEdits`, `plan`, `auto`, `bypassPermissions`); other harnesses map the
/// nearest equivalent in their `run_turn`. `auto` is distinct from `acceptEdits`
/// (it's Claude's balanced default mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    /// Prompt for every action. (`default`)
    Ask,
    /// Auto-accept file edits; still prompt for other tools. (`acceptEdits`)
    AcceptEdits,
    /// Read/plan only — propose without executing. (`plan`)
    Plan,
    /// Claude Code's default balanced auto mode. (`auto`)
    Auto,
    /// No prompts at all. (`bypassPermissions`)
    Bypass,
}

impl PermissionMode {
    /// The stable wire id (what the UI stores and sends). Matches Claude Code's
    /// `--permission-mode` value so no per-harness remap is needed for Claude.
    pub fn id(self) -> &'static str {
        match self {
            PermissionMode::Ask => "default",
            PermissionMode::AcceptEdits => "acceptEdits",
            PermissionMode::Plan => "plan",
            PermissionMode::Auto => "auto",
            PermissionMode::Bypass => "bypassPermissions",
        }
    }

    /// Menu label (matches the Claude Code composer wording).
    pub fn label(self) -> &'static str {
        match self {
            PermissionMode::Ask => "Ask permissions",
            PermissionMode::AcceptEdits => "Accept edits",
            PermissionMode::Plan => "Plan mode",
            PermissionMode::Auto => "Auto mode",
            PermissionMode::Bypass => "Bypass permissions",
        }
    }

    /// Parse a wire id back to a mode. Unknown ids fall back to `None` so the
    /// caller can apply its own default.
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "default" => Some(PermissionMode::Ask),
            "acceptEdits" => Some(PermissionMode::AcceptEdits),
            "plan" => Some(PermissionMode::Plan),
            "auto" => Some(PermissionMode::Auto),
            "bypassPermissions" => Some(PermissionMode::Bypass),
            _ => None,
        }
    }
}

/// One selectable value in a composer toggle (id + human label). Ids are
/// `&'static str` so both the shared `PermissionMode` and per-harness reasoning
/// lists can produce them without allocation.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OptionChoice {
    pub id: &'static str,
    pub label: &'static str,
}

/// The toggle vocabulary a harness supports, sent to the UI so it can render
/// only valid choices and pre-select the harness's defaults. An empty list for
/// an axis means "this harness has no such control" and the UI hides it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessOptions {
    pub permission_modes: Vec<OptionChoice>,
    pub default_permission_mode: Option<&'static str>,
    pub reasoning_levels: Vec<OptionChoice>,
    pub default_reasoning_level: Option<&'static str>,
}

impl HarnessOptions {
    /// A harness with neither control (the trait default).
    pub fn none() -> Self {
        Self {
            permission_modes: Vec::new(),
            default_permission_mode: None,
            reasoning_levels: Vec::new(),
            default_reasoning_level: None,
        }
    }

    pub fn with_permission_modes(
        mut self,
        modes: &[PermissionMode],
        default: PermissionMode,
    ) -> Self {
        self.permission_modes = modes
            .iter()
            .map(|m| OptionChoice {
                id: m.id(),
                label: m.label(),
            })
            .collect();
        self.default_permission_mode = Some(default.id());
        self
    }

    /// Set the reasoning list from harness-owned `(id, label)` pairs. Unlike
    /// permission modes, reasoning vocabulary isn't shared — Claude's `--effort`
    /// tiers and a future Codex's `reasoning_effort` would differ — so each
    /// harness passes its own choices and interprets the chosen id in its
    /// `run_turn`. (Only Claude advertises reasoning levels today.)
    pub fn with_reasoning_levels(
        mut self,
        levels: &[(&'static str, &'static str)],
        default: &'static str,
    ) -> Self {
        self.reasoning_levels = levels
            .iter()
            .map(|(id, label)| OptionChoice { id, label })
            .collect();
        self.default_reasoning_level = Some(default);
        self
    }
}
