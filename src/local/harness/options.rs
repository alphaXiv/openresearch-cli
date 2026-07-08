//! Cross-harness turn options — the permission mode and reasoning level a chat
//! session runs under. These are the shared vocabulary the UI toggles speak;
//! each harness advertises which values it supports (`options()`) and maps the
//! chosen value onto its own CLI (in its `run_turn`).
//!
//! The wire ids are stable, harness-agnostic strings (`"auto"`, `"high"`). A
//! harness that doesn't support a given axis simply lists no options for it,
//! and the composer hides that control.

use serde::{Deserialize, Serialize};

/// How much the harness should defer to the user before acting. Mirrors Claude
/// Code's `--permission-mode`; other harnesses map the nearest equivalent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    /// Auto-accept file edits; still prompt for other tools. (`acceptEdits`)
    Auto,
    /// Read/plan only — propose without executing. (`plan`)
    Plan,
    /// Normal prompting for everything. (`default`)
    Default,
    /// No prompts at all. (`bypassPermissions`)
    Bypass,
}

impl PermissionMode {
    /// The stable wire id (what the UI stores and sends).
    pub fn id(self) -> &'static str {
        match self {
            PermissionMode::Auto => "auto",
            PermissionMode::Plan => "plan",
            PermissionMode::Default => "default",
            PermissionMode::Bypass => "bypass",
        }
    }

    /// Short label for the composer toggle.
    pub fn label(self) -> &'static str {
        match self {
            PermissionMode::Auto => "Auto",
            PermissionMode::Plan => "Plan",
            PermissionMode::Default => "Default",
            PermissionMode::Bypass => "Bypass",
        }
    }

    /// Parse a wire id back to a mode. Unknown ids fall back to `None` so the
    /// caller can apply its own default.
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "auto" => Some(PermissionMode::Auto),
            "plan" => Some(PermissionMode::Plan),
            "default" => Some(PermissionMode::Default),
            "bypass" => Some(PermissionMode::Bypass),
            _ => None,
        }
    }
}

/// How much the model should think before answering. Portable across harnesses:
/// Claude maps it to thinking keywords, Codex to `reasoning_effort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReasoningLevel {
    Low,
    Medium,
    High,
}

impl ReasoningLevel {
    pub fn id(self) -> &'static str {
        match self {
            ReasoningLevel::Low => "low",
            ReasoningLevel::Medium => "medium",
            ReasoningLevel::High => "high",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ReasoningLevel::Low => "Low",
            ReasoningLevel::Medium => "Medium",
            ReasoningLevel::High => "High",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "low" => Some(ReasoningLevel::Low),
            "medium" => Some(ReasoningLevel::Medium),
            "high" => Some(ReasoningLevel::High),
            _ => None,
        }
    }
}

/// One selectable value in a composer toggle (id + human label).
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

    pub fn with_reasoning_levels(
        mut self,
        levels: &[ReasoningLevel],
        default: ReasoningLevel,
    ) -> Self {
        self.reasoning_levels = levels
            .iter()
            .map(|l| OptionChoice {
                id: l.id(),
                label: l.label(),
            })
            .collect();
        self.default_reasoning_level = Some(default.id());
        self
    }
}
