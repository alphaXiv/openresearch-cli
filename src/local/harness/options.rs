//! Cross-harness turn options — the permission mode and reasoning level a chat
//! session runs under. These are the vocabulary the UI toggles speak; each
//! harness advertises which values it supports (`options()`) and maps the
//! chosen value onto its own CLI (in its `run_turn`).
//!
//! The two axes are modeled differently on purpose:
//!
//! * Permission mode is a *shared* enum — the concept (ask / accept-edits /
//!   plan / auto / bypass) is common enough to name once. Its wire ids are
//!   harness-agnostic (`ask` / `accept-edits` / `plan` / `auto` / `bypass`);
//!   each harness maps the enum onto its own control surface in `run_turn`
//!   (Claude → `--permission-mode`, Codex → `--sandbox` policy). The ids were
//!   neutralized off Claude's `--permission-mode` spelling once Codex landed and
//!   its sandbox policies didn't map onto Claude's strings — see the store data
//!   migration in `store.rs` that rewrites the old spellings.
//! * Reasoning level is deliberately NOT shared — Claude's tiers (`low`…`max`,
//!   via `--effort`) and Codex's (`low`/`medium`/`high`, via
//!   `model_reasoning_effort`) genuinely differ — so each harness owns its own
//!   `OptionChoice` list and interprets the chosen id itself.
//!
//! A harness that doesn't support an axis lists nothing for it, and the composer
//! hides that control.

use serde::{Deserialize, Serialize};

/// How much the harness should defer to the user before acting. The wire ids
/// are harness-agnostic (`ask`, `accept-edits`, `plan`, `auto`, `bypass`); each
/// harness maps the enum onto its own control surface in `run_turn` (Claude →
/// `--permission-mode`, Codex → `--sandbox`). `auto` is distinct from
/// `accept-edits` (it's Claude's balanced default mode).
///
/// Not every harness supports every mode — a harness advertises its supported
/// subset via `options()` and the composer only offers those. `plan`, for
/// instance, is Claude + OpenCode + Codex (each with its own machinery): Claude's
/// plan mode pairs with a `PreToolUse` hook so read-only `orx` inspection still
/// runs (see `plan_gate`); OpenCode has a native read-only `plan` agent; Codex
/// attaches a native `collaborationMode` mask over the app-server (its own
/// plan.md template — restriction is prompt-level, not sandbox-level).
/// `ask`/`accept-edits` are the modes not every harness carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    /// Prompt for every action. (`ask`)
    Ask,
    /// Auto-accept file edits; still prompt for other tools. (`accept-edits`)
    AcceptEdits,
    /// Read/plan only — propose without executing. (`plan`)
    Plan,
    /// Claude Code's default balanced auto mode. (`auto`)
    Auto,
    /// No prompts at all. (`bypass`)
    Bypass,
}

impl PermissionMode {
    /// The stable, harness-agnostic wire id (what the UI stores and sends). Each
    /// harness maps this to its own CLI/API in `run_turn`.
    pub fn id(self) -> &'static str {
        match self {
            PermissionMode::Ask => "ask",
            PermissionMode::AcceptEdits => "accept-edits",
            PermissionMode::Plan => "plan",
            PermissionMode::Auto => "auto",
            PermissionMode::Bypass => "bypass",
        }
    }

    /// Menu label shown in the composer's permission-mode toggle.
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
            "ask" => Some(PermissionMode::Ask),
            "accept-edits" => Some(PermissionMode::AcceptEdits),
            "plan" => Some(PermissionMode::Plan),
            "auto" => Some(PermissionMode::Auto),
            "bypass" => Some(PermissionMode::Bypass),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire ids are the store/UI contract — pin them so a rename is a
    /// deliberate, test-breaking change (and a reminder to add a data migration).
    #[test]
    fn wire_ids_are_the_neutralized_spelling() {
        assert_eq!(PermissionMode::Ask.id(), "ask");
        assert_eq!(PermissionMode::AcceptEdits.id(), "accept-edits");
        assert_eq!(PermissionMode::Plan.id(), "plan");
        assert_eq!(PermissionMode::Auto.id(), "auto");
        assert_eq!(PermissionMode::Bypass.id(), "bypass");
    }

    #[test]
    fn from_id_round_trips_every_mode() {
        for mode in [
            PermissionMode::Ask,
            PermissionMode::AcceptEdits,
            PermissionMode::Plan,
            PermissionMode::Auto,
            PermissionMode::Bypass,
        ] {
            assert_eq!(PermissionMode::from_id(mode.id()), Some(mode));
        }
    }

    #[test]
    fn from_id_rejects_the_old_claude_spellings_and_junk() {
        // The pre-migration spellings must NOT parse — a stale row is normalized
        // by the store migration, not silently reinterpreted here.
        for old in [
            "default",
            "acceptEdits",
            "bypassPermissions",
            "",
            "nonsense",
        ] {
            assert_eq!(PermissionMode::from_id(old), None, "{old} should not parse");
        }
    }

    #[test]
    fn permission_mode_serde_uses_kebab_ids() {
        // The enum is serialized directly in some payloads; its serde form must
        // match the wire ids (kebab-case), not the Rust variant names.
        let json = serde_json::to_string(&PermissionMode::AcceptEdits).unwrap();
        assert_eq!(json, "\"accept-edits\"");
        let back: PermissionMode = serde_json::from_str("\"bypass\"").unwrap();
        assert_eq!(back, PermissionMode::Bypass);
    }
}
