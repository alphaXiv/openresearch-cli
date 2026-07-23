//! Claude Code harness.
//!
//! Chat: one *resident* `claude --print --input-format stream-json` child per
//! chat session (`local::claude::ClaudeHost`), reused across turns — each turn
//! sends one user message and folds the child's stream-json output until a
//! `result` event. The child persists (stable `session_id`, stdin held open),
//! collapsing the old spawn-per-turn overhead; a config change (permission mode
//! / effort / bridge), interrupt, or crash respawns it with `--resume`. The
//! playbook rides `--append-system-prompt-file`; the permission mode is
//! `--permission-mode` from the session's setting (`auto`/`bypass` — see
//! `options`). AskUserQuestion / ExitPlanMode surface as interactive cards: the
//! turn ends on them and the user's answer resumes the session — except in plan
//! mode, where the mcp-gate bridge holds both open mid-turn and the answer
//! continues the same turn.
//!
//! Detection: `~/.claude.json` carries the signed-in OAuth account (no secrets
//! read); `ANTHROPIC_API_KEY` is the api-key fallback.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value;

use super::detect::{bin_version, find_on_path, nonempty_str, read_json, HarnessInfo};
use super::options::{HarnessOptions, PermissionMode};
use super::{Harness, ResumeAction};
use crate::error::{anyhow, Result};
use crate::local::chat::{
    ContextUsage, PromptAnswer, ResumeCtx, TurnCtx, WirePart, WirePrompt, WireQuestionOption,
    WireToolState,
};
use crate::local::claude::{SpawnConfig, SpawnSpec, TurnEvent};
use crate::local::opencode::ensure_playbook;

/// Each harness runs directly (its own CLI, the user's own login), so its
/// model list is its own: static ids for the Claude Code CLI.
const CLAUDE_MODELS: [&str; 4] = [
    "claude-fable-5",
    "claude-sonnet-5",
    "claude-opus-4-8",
    "claude-haiku-4-5",
];

/// Claude Code's `--effort` tiers (id == the CLI value). `xhigh`/`max` are
/// Claude-specific — the reasoning vocabulary is per-harness, not global.
const CLAUDE_EFFORT_LEVELS: [(&str, &str); 5] = [
    ("low", "Low"),
    ("medium", "Medium"),
    ("high", "High"),
    ("xhigh", "XHigh"),
    ("max", "Max"),
];

pub struct ClaudeCode;

/// `claude` on PATH, else the common install drop locations.
pub(crate) fn find_claude() -> Option<PathBuf> {
    find_on_path("claude").or_else(|| {
        let home = dirs::home_dir()?;
        [".claude/local/claude", ".local/bin/claude"]
            .iter()
            .map(|rel| home.join(rel))
            .find(|c| c.is_file())
    })
}

#[async_trait]
impl Harness for ClaudeCode {
    fn id(&self) -> &'static str {
        "claude-code"
    }

    fn name(&self) -> &'static str {
        "Claude Code"
    }

    fn supports_chat(&self) -> bool {
        true
    }

    async fn detect(&self) -> Option<HarnessInfo> {
        let mut info = HarnessInfo::new(self.id(), self.name());
        if let Some(bin) = find_claude() {
            info.installed = true;
            info.version = bin_version(&bin).await;
            info.bin_path = Some(bin.to_string_lossy().into_owned());
        }
        // ~/.claude.json carries the signed-in OAuth account (no secrets read).
        if let Some(cfg) = dirs::home_dir().and_then(|h| read_json(h.join(".claude.json"))) {
            if let Some(acct) = cfg.get("oauthAccount") {
                info.authenticated = true;
                info.auth_method = Some("oauth");
                info.account = nonempty_str(acct, "emailAddress");
                info.org = nonempty_str(acct, "organizationName");
                info.plan = match nonempty_str(acct, "billingType").as_deref() {
                    Some("stripe_subscription") => Some("Subscription".to_string()),
                    Some(other) => Some(other.to_string()),
                    None => None,
                };
            }
        }
        if !info.authenticated && std::env::var("ANTHROPIC_API_KEY").is_ok_and(|v| !v.is_empty()) {
            info.authenticated = true;
            info.auth_method = Some("apiKey");
        }

        info.agent_ready = info.installed && info.authenticated;
        if info.agent_ready {
            info = info.with_models(&CLAUDE_MODELS);
        } else {
            info.agent_note = Some(
                "Install Claude Code and sign in (`claude`) to chat with it here.".to_string(),
            );
        }
        Some(info)
    }

    async fn run_turn(&self, ctx: &mut TurnCtx) -> Result<()> {
        run_turn(ctx).await
    }

    fn options(&self) -> HarnessOptions {
        // Plan + Auto + Bypass. Headless `claude --print` has no interactive
        // approval, so `ask`/`accept-edits` can't grant a blocked tool (they just
        // deny) — those stay out.
        //   * Plan  — read/propose only: file edits stay blocked until the user
        //     approves the plan (via the ExitPlanMode card). Plan mode would
        //     normally also gate `Bash(orx …)`, which would break planning — the
        //     agent plans by *inspecting* prior runs/logs/evidence via read-only
        //     `orx`. A `PreToolUse` hook (wired in `run_turn` only for this mode,
        //     via `write_plan_settings`) lets read-only `orx` verbs through while
        //     launches (`orx exp run`, `instance`, …) stay gated. See `plan_gate`.
        //   * Auto  — the balanced default; runs tools without prompting.
        //   * Bypass— runs everything, no sandbox.
        HarnessOptions::none()
            .with_permission_modes(
                &[
                    PermissionMode::Plan,
                    PermissionMode::Auto,
                    PermissionMode::Bypass,
                ],
                PermissionMode::Auto,
            )
            // Claude Code's `--effort` tiers (default `high` on current models).
            .with_reasoning_levels(&CLAUDE_EFFORT_LEVELS, "high")
    }

    /// Two resume paths. A card the permission bridge surfaced mid-turn
    /// (`native_id` set) settles the held bridge request — the still-running
    /// turn unblocks in place ([`ResumeAction::Handled`]), except plan
    /// approval, which interrupts the paused plan turn and resumes via a new
    /// message under the approved mode. An end-turn card (no `native_id`)
    /// resumes by sending a *new user message* under `--resume` (see
    /// `run_turn`); a denied permission is the one case with no resume.
    async fn resume_from_prompt(
        &self,
        ctx: &ResumeCtx,
        prompt: &WirePrompt,
        answer: &PromptAnswer,
    ) -> Result<ResumeAction> {
        if let Some(native_id) = &prompt.native_id {
            // The bridge request lives inside a running turn; once that turn is
            // gone the card is stale. Normally `PendingGuard` resolves it at
            // turn teardown, but a process crash/restart skips that — leaving
            // a zombie card that renders actionable and swallows every answer
            // forever. Collapse it store-side before reporting the miss.
            if !ctx.is_busy().await {
                ctx.host
                    .resolve_zombie_prompt(&ctx.session_id, &answer.prompt_id);
                return Err(anyhow!("this approval is no longer pending"));
            }
            let note = answer.note.as_deref().filter(|s| !s.trim().is_empty());
            return match (prompt.kind.as_str(), answer.approve) {
                // Mid-turn tool approval: answer the held request; the turn
                // keeps streaming. The CLI requires updatedInput on an allow —
                // echo the card's recorded input.
                ("permission", true) => {
                    ctx.host.settle_permission(
                        native_id,
                        crate::local::chat::PermissionDecision::Allow {
                            updated_input: prompt.tool_input.clone(),
                        },
                    )?;
                    Ok(ResumeAction::Handled)
                }
                ("permission", false) => {
                    let message = match note {
                        Some(note) => format!(
                            "The user denied this action: {note}. Do not retry it; adjust course."
                        ),
                        None => "The user denied this action. Do not retry it; adjust course."
                            .to_string(),
                    };
                    ctx.host.settle_permission(
                        native_id,
                        crate::local::chat::PermissionDecision::Deny { message },
                    )?;
                    Ok(ResumeAction::Handled)
                }
                // Deny the held ExitPlanMode. With a note it's a revision
                // request — the model revises the plan in the same turn. With
                // no note it's a plain REJECTION (the strip's Reject button):
                // tell the model to stop, not to improvise a revision (or a
                // "what should change?" question card). The wording is
                // `synthesize_resume`'s plan-deny arm verbatim — one source
                // for both delivery shapes.
                ("plan", false) => {
                    let (message, _) = synthesize_resume("plan", answer);
                    ctx.host.settle_permission(
                        native_id,
                        crate::local::chat::PermissionDecision::Deny { message },
                    )?;
                    Ok(ResumeAction::Handled)
                }
                // Plan approval: don't settle the held request — the paused
                // plan turn gets interrupted (respond()'s SendMessage arm) and
                // replaced by a fresh implementation turn under the approved
                // mode, reusing the proven --resume machinery. The drained
                // bridge request is denied into the dying child, harmlessly.
                ("plan", true) => {
                    let (text, mode) = synthesize_resume("plan", answer);
                    Ok(ResumeAction::SendMessage { text, mode })
                }
                // Mid-turn question (a bridge-held AskUserQuestion): the held
                // tool call is denied with the user's answer as the message —
                // the model reads the answer from the denial and continues the
                // same turn. (Allowing the tool instead would run it headless,
                // which returns no answer — the model would guess and move on
                // rather than block; that's the bug this arm exists to avoid.)
                ("question", _) => {
                    let (text, _) = synthesize_resume("question", answer);
                    if text.trim().is_empty() {
                        return Err(anyhow!("select an option (or add a note) to answer"));
                    }
                    ctx.host.settle_permission(
                        native_id,
                        crate::local::chat::PermissionDecision::Deny {
                            message: format!(
                                "The user answered: {text}. Treat this as their answer and \
                                 continue — do not ask this question again. (Only the first \
                                 question of the call was shown; ask any others separately.)"
                            ),
                        },
                    )?;
                    Ok(ResumeAction::Handled)
                }
                _ => Err(anyhow!("unsupported prompt kind for a bridge card")),
            };
        }

        // A denied permission closes the card without resuming; every other
        // answer continues the session.
        if prompt.kind == "permission" && !answer.approve {
            return Ok(ResumeAction::Nothing);
        }
        // Likewise a note-less plan REJECTION on an end-turn card: the turn is
        // already over — resuming just to say "stop" would end in fresh text
        // that `should_synthesize_plan` turns into ANOTHER card, so Reject
        // could never dismiss the strip. Close the card with no resume.
        if prompt.kind == "plan"
            && !answer.approve
            && answer.note.as_deref().is_none_or(|s| s.trim().is_empty())
        {
            return Ok(ResumeAction::Nothing);
        }
        let (text, mode) = synthesize_resume(&prompt.kind, answer);
        // Reject an empty resume (e.g. a question answered with no selection and
        // no note) so `respond` leaves the card actionable.
        if text.trim().is_empty() {
            return Err(anyhow!("no answer provided"));
        }
        Ok(ResumeAction::SendMessage { text, mode })
    }

    fn config_home(&self) -> Option<PathBuf> {
        Some(dirs::home_dir()?.join(".claude"))
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
        Some(super::CLAUDE_SKILL)
    }

    fn session_skills_dir(&self) -> Option<&'static str> {
        Some(".claude/skills")
    }
}

/// Session mode → Claude Code `--permission-mode` value. The shared wire ids are
/// harness-agnostic (`ask`/`accept-edits`/`bypass`), so this is where the enum
/// is spelled back into Claude's own CLI vocabulary; `Auto` is the default when
/// the session hasn't picked a mode.
pub(crate) fn claude_permission_mode(mode: Option<PermissionMode>) -> &'static str {
    match mode.unwrap_or(PermissionMode::Auto) {
        PermissionMode::Ask => "default",
        PermissionMode::AcceptEdits => "acceptEdits",
        PermissionMode::Plan => "plan",
        PermissionMode::Auto => "auto",
        PermissionMode::Bypass => "bypassPermissions",
    }
}

/// Path (relative to the worktree) of the plan-mode settings file we write and
/// pass via `--settings`. Lives under the same agent dir as the playbook, which
/// is already git-excluded.
const PLAN_SETTINGS_REL: &str = ".openresearch/agent/claude-plan-settings.json";

/// Path (relative to the worktree) of the plan-mode MCP config wiring the
/// `orx mcp-gate` permission bridge. Same git-excluded agent dir.
const MCP_CONFIG_REL: &str = ".openresearch/agent/claude-mcp.json";

/// Write the plan-mode `--settings` file into `repo` and return its path. The
/// file registers `PreToolUse` hooks running `orx plan-gate` (this same
/// binary): on `Bash` it allows read-only inspection through plan mode's gate,
/// and on `ExitPlanMode` it forces an `ask` — headless plan mode otherwise
/// SELF-approves the call ("User has approved exiting plan mode", nobody
/// asked; verified on claude 2.1.197) and starts editing. The `ask` routes
/// plan approval to the permission bridge card. See `plan_gate`.
///
/// The hook command is this executable's absolute path, so it resolves without
/// depending on `orx` being on Claude's `PATH`.
pub(crate) fn write_plan_settings(repo: &std::path::Path) -> Result<PathBuf> {
    let orx = std::env::current_exe()
        .map_err(|e| anyhow!("cannot resolve orx binary path for plan-mode hook: {e}"))?;
    let hook = serde_json::json!([{
        "type": "command",
        "command": format!(
            "{} plan-gate",
            crate::jobs::ssh::sh_quote(&orx.to_string_lossy())
        ),
    }]);
    let settings = serde_json::json!({
        "hooks": {
            "PreToolUse": [
                { "matcher": "Bash", "hooks": hook },
                { "matcher": "ExitPlanMode", "hooks": hook },
            ],
        }
    });
    let path = repo.join(PLAN_SETTINGS_REL);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow!("cannot create {}: {e}", parent.display()))?;
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&settings).unwrap())
        .map_err(|e| anyhow!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

/// Write the per-spawn `--mcp-config` file pointing Claude at `orx mcp-gate`
/// (this same binary) and return its path. The bridge's env block carries the
/// `orx up` port, the session id, and a fresh per-child token minted at spawn —
/// everything the resident bridge needs to relay permission requests back to
/// the running server for the child's whole life.
pub(crate) fn write_mcp_config(
    repo: &std::path::Path,
    up_port: u16,
    session_id: &str,
    token: &str,
) -> Result<PathBuf> {
    let orx = std::env::current_exe()
        .map_err(|e| anyhow!("cannot resolve orx binary path for the mcp bridge: {e}"))?;
    let config = serde_json::json!({
        "mcpServers": {
            "orx": {
                "type": "stdio",
                "command": orx.to_string_lossy(),
                "args": ["mcp-gate"],
                "env": {
                    "ORX_UP_PORT": up_port.to_string(),
                    "ORX_SESSION_ID": session_id,
                    "ORX_GATE_TOKEN": token,
                },
            },
        }
    });
    let path = repo.join(MCP_CONFIG_REL);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow!("cannot create {}: {e}", parent.display()))?;
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&config).unwrap())
        .map_err(|e| anyhow!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

/// Session reasoning id → Claude Code `--effort` value. The composer only
/// offers ids from `CLAUDE_EFFORT_LEVELS`, so an unrecognized/absent value just
/// omits the flag and lets the CLI apply its own default (`high`).
fn claude_effort(level: Option<&str>) -> Option<&str> {
    let level = level?;
    CLAUDE_EFFORT_LEVELS
        .iter()
        .any(|(id, _)| *id == level)
        .then_some(level)
}

/// The follow-up message + resume mode for an answered Claude prompt — Claude's
/// resume strategy: a prompt ends the turn and the answer becomes a *new user
/// message* that continues via `--resume`. `resume_mode` on the answer is a
/// harness-agnostic wire id; unknown/absent ids fall through to the per-kind
/// default (or the session's mode, applied downstream). The question arm is
/// also reused as a plain text builder by the bridge's mid-turn question
/// resume (the denial message that carries the answer).
pub(crate) fn synthesize_resume(
    kind: &str,
    req: &PromptAnswer,
) -> (String, Option<PermissionMode>) {
    let note = req.note.as_deref().filter(|s| !s.trim().is_empty());
    let chosen = req.resume_mode.as_deref().and_then(PermissionMode::from_id);
    match kind {
        "plan" if req.approve => {
            let mut text = "The user approved the plan. Proceed with implementing it.".to_string();
            if let Some(note) = note {
                text.push_str(&format!("\n\nAdditional guidance: {note}"));
            }
            // Approving a plan means leaving plan mode; default to `auto`.
            (text, chosen.or(Some(PermissionMode::Auto)))
        }
        "plan" => {
            // Stay in plan mode. With a note it's a revision request; without
            // one it's a plain rejection — stop, don't guess at revisions.
            let text = note
                .map(|n| format!("Keep refining the plan: {n}"))
                .unwrap_or_else(|| {
                    "The user rejected this plan. Stop planning and wait for \
                     further instructions."
                        .to_string()
                });
            (text, Some(PermissionMode::Plan))
        }
        "permission" => {
            // Approving a blocked tool must resume under a mode that actually
            // *grants* it. Claude's `--permission-mode` is coarse: `acceptEdits`
            // only auto-approves file edits, so it leaves a Bash (or any
            // non-edit) denial in place — the tool is denied again and the card
            // re-appears in a loop. `bypass` is the only mode that lets the
            // previously-blocked tool through, so that's the default for an
            // approval (a caller can still override via `resume_mode`). Verified
            // against the CLI: acceptEdits re-denies Bash, bypass clears it.
            let text = "The user approved that action. Continue.".to_string();
            (text, chosen.or(Some(PermissionMode::Bypass)))
        }
        // question (or anything else): feed the selection back as the user's reply.
        _ => {
            let mut text = if req.answers.is_empty() {
                note.unwrap_or("").to_string()
            } else {
                req.answers.join(", ")
            };
            if let (false, Some(note)) = (req.answers.is_empty(), note) {
                text.push_str(&format!("\n\n{note}"));
            }
            (text, None)
        }
    }
}

/// Whether a finished plan-mode turn needs a synthesized plan card: the model
/// presented its plan as plain text without calling ExitPlanMode (and without
/// asking a question), and the turn didn't error. Without a card the user is
/// stranded — only a plan-card answer switches the resume mode, so a plain
/// chat reply would resume still in plan mode. A trivial Q&A turn in plan mode
/// also gets a card: in plan mode the only exit *is* a plan answer, so the
/// card is always the recourse.
pub(crate) fn should_synthesize_plan(
    plan_mode: bool,
    saw_prompt: bool,
    errored: bool,
    final_text: &str,
) -> bool {
    plan_mode && !saw_prompt && !errored && !final_text.trim().is_empty()
}

/// ExitPlanMode → a `plan` prompt (its `input.plan` is the proposed markdown).
fn plan_prompt(name: &str, input: Option<&Value>) -> Option<WirePrompt> {
    if name != "ExitPlanMode" {
        return None;
    }
    let plan = input
        .and_then(|i| i.get("plan"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Some(WirePrompt {
        kind: "plan".into(),
        plan: Some(plan),
        ..Default::default()
    })
}

/// AskUserQuestion → a `question` prompt. Claude's schema is
/// `{questions: [{question, header, options: [{label, description}], multiSelect}]}`;
/// we surface the first question (the composer answers one at a time). Also
/// used by the plan-mode bridge (`ChatHost::request_permission`, via the
/// harness re-export) to build the held mid-turn question card.
pub(crate) fn question_prompt(name: &str, input: Option<&Value>) -> Option<WirePrompt> {
    if name != "AskUserQuestion" {
        return None;
    }
    let q = input
        .and_then(|i| i.get("questions"))
        .and_then(Value::as_array)
        .and_then(|qs| qs.first())?;
    let options = q
        .get("options")
        .and_then(Value::as_array)
        .map(|opts| {
            opts.iter()
                .filter_map(|o| {
                    Some(WireQuestionOption {
                        label: o.get("label").and_then(Value::as_str)?.to_string(),
                        description: o
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(WirePrompt {
        kind: "question".into(),
        question: q
            .get("question")
            .and_then(Value::as_str)
            .map(str::to_string),
        header: q.get("header").and_then(Value::as_str).map(str::to_string),
        options,
        multi_select: q
            .get("multiSelect")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        ..Default::default()
    })
}

/// Claude's tool inputs are snake_case; the UI summarizes via `filePath`.
fn normalize_input(input: &Value) -> Value {
    let mut input = input.clone();
    if let Some(obj) = input.as_object_mut() {
        if let Some(fp) = obj.get("file_path").cloned() {
            obj.entry("filePath").or_insert(fp);
        }
    }
    input
}

/// tool_result content: plain string or [{type: "text", text}] blocks.
fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The per-turn state `apply_event` folds each stream-json line into. Kept
/// store-free so the caller (not the fold) owns every side effect — the native
/// session id is applied per event by `run_turn`, and every flush happens there
/// too, which is what lets the fold run against a bare `TurnCtx::test_stub()` in
/// the fixture tests.
#[derive(Default)]
struct TurnState {
    /// Whether this child was spawned with the mcp-gate bridge active. With the
    /// bridge on, ExitPlanMode / AskUserQuestion come from the bridge (held,
    /// mid-turn-answerable), so their `tool_use` renders nothing.
    bridge_active: bool,
    /// The `result` event has landed — the turn is over.
    saw_result: bool,
    /// An interactive card was surfaced this turn (suppresses the synthesized
    /// plan card — see `should_synthesize_plan`).
    saw_prompt: bool,
    /// The turn ended with a genuine failure (drives the error path).
    turn_errored: bool,
    /// The last non-empty assistant text block — the plan, if the model wrote
    /// one as plain text.
    last_text: String,
    /// The native session id from the latest `system/init` or `result` (the
    /// caller applies it to the store per event).
    native_session_id: Option<String>,
    /// The in-flight assistant message id from the stream's `message_start` —
    /// deltas carry only a block `index`, so this is what keys them to the
    /// same `{mid}-{index}` part ids the final complete `assistant` event
    /// upserts.
    stream_mid: Option<String>,
}

/// Fold one stream-json output object into the turn's transcript + `TurnState`.
/// Pure w.r.t. the store — touches only `ctx.assistant.parts` (via the TurnCtx
/// helpers) and `state` — so it is fixture-tested against `TurnCtx::test_stub()`.
/// Returns `true` when this event is the terminal `result` (the caller stops
/// the recv loop). Native-session-id application and flushing are the caller's
/// job, keeping this store-free.
fn apply_event(ctx: &mut TurnCtx, state: &mut TurnState, event: &Value) -> bool {
    match event.get("type").and_then(Value::as_str) {
        // Partial-message deltas (opt-in via --include-partial-messages): the
        // text/thinking streams token by token instead of landing as one block
        // when the complete `assistant` event arrives. Deltas build a part
        // under the same `{mid}-{index}` id that the final event upserts, so
        // the authoritative full text simply overwrites the accumulated one.
        // That overwrite (and part ordering) leans on two stream-protocol
        // invariants: the stream's message id equals the final assistant
        // event's, and a block's `index` is its position in the final content
        // array, with blocks streamed in ascending order.
        Some("stream_event") => {
            // A subagent's nested stream (parent_tool_use_id set) would
            // interleave its text into the transcript — main loop only.
            if !event.get("parent_tool_use_id").is_none_or(|v| v.is_null()) {
                return false;
            }
            let inner = event.get("event").unwrap_or(&Value::Null);
            match inner.get("type").and_then(Value::as_str) {
                Some("message_start") => {
                    if let Some(mid) = inner.pointer("/message/id").and_then(Value::as_str) {
                        state.stream_mid = Some(mid.to_string());
                    }
                }
                Some("content_block_delta") => {
                    let (Some(mid), Some(index)) = (
                        state.stream_mid.as_deref(),
                        inner.get("index").and_then(Value::as_u64),
                    ) else {
                        return false;
                    };
                    let id = format!("{mid}-{index}");
                    let delta = inner.get("delta").unwrap_or(&Value::Null);
                    let (field, reasoning) = match delta.get("type").and_then(Value::as_str) {
                        Some("text_delta") => ("text", false),
                        Some("thinking_delta") => ("thinking", true),
                        _ => return false,
                    };
                    if let Some(text) = delta.get(field).and_then(Value::as_str) {
                        if !ctx.assistant.parts.iter().any(|p| p.id == id) {
                            ctx.upsert_part(if reasoning {
                                WirePart::reasoning(id.clone(), "")
                            } else {
                                WirePart::text(id.clone(), "")
                            });
                        }
                        ctx.append_part_text(&id, text);
                    }
                }
                _ => {}
            }
        }
        Some("system") => {
            if event.get("subtype").and_then(Value::as_str) == Some("init") {
                if let Some(sid) = event.get("session_id").and_then(Value::as_str) {
                    state.native_session_id = Some(sid.to_string());
                }
            }
        }
        Some("assistant") => {
            let mid = event
                .pointer("/message/id")
                .and_then(Value::as_str)
                .unwrap_or("m")
                .to_string();
            let blocks = event
                .pointer("/message/content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for (i, block) in blocks.iter().enumerate() {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                        if !text.trim().is_empty() {
                            state.last_text = text.to_string();
                        }
                        ctx.upsert_part(WirePart::text(format!("{mid}-{i}"), text));
                    }
                    Some("thinking") => {
                        let text = block.get("thinking").and_then(Value::as_str).unwrap_or("");
                        ctx.upsert_part(WirePart::reasoning(format!("{mid}-{i}"), text));
                    }
                    Some("tool_use") => {
                        let id = block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or(&format!("{mid}-{i}"))
                            .to_string();
                        let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                        let input = block.get("input");
                        // ExitPlanMode / AskUserQuestion surface as interactive
                        // prompt cards instead of plain tool rows, and the user's
                        // choice resumes the session. With the bridge active,
                        // BOTH cards come from the bridge instead (held,
                        // mid-turn-answerable) and the tool_use renders NOTHING:
                        // a tool row would duplicate the card — and the denial
                        // that carries the user's answer back would paint it as a
                        // spurious error row once the tool_result lands.
                        if state.bridge_active && matches!(name, "ExitPlanMode" | "AskUserQuestion")
                        {
                            continue;
                        }
                        if let Some(prompt) =
                            plan_prompt(name, input).or_else(|| question_prompt(name, input))
                        {
                            state.saw_prompt = true;
                            ctx.upsert_part(WirePart::prompt(id, prompt));
                        } else {
                            ctx.upsert_part(WirePart {
                                id,
                                kind: "tool".into(),
                                text: None,
                                tool: Some(name.to_string()),
                                state: Some(WireToolState {
                                    status: "running".into(),
                                    input: input.map(normalize_input),
                                    output: None,
                                    error: None,
                                    title: None,
                                }),
                                prompt: None,
                                children: Vec::new(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            // Per-message usage gives live updates during multi-step turns; the
            // window arrives later on `result`, so report the token count only.
            // A subagent's message is a top-level `assistant` event with
            // `parent_tool_use_id` set — its smaller count must not overwrite
            // (latest-wins) the main session's occupancy, so skip its usage.
            let is_subagent = !event.get("parent_tool_use_id").is_none_or(Value::is_null);
            if !is_subagent {
                if let Some(used) = claude_used_tokens(event.pointer("/message/usage")) {
                    ctx.report_usage(ContextUsage {
                        used_tokens: used,
                        context_window: None,
                    });
                }
            }
        }
        Some("user") => {
            // Synthetic tool-result turns: complete the matching tool part.
            let blocks = event
                .pointer("/message/content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for block in &blocks {
                if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                    continue;
                }
                let Some(tool_id) = block.get("tool_use_id").and_then(Value::as_str) else {
                    continue;
                };
                let is_error = block
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let text = block.get("content").map(result_text).unwrap_or_default();
                if let Some(part) = ctx.assistant.parts.iter_mut().find(|p| p.id == tool_id) {
                    if let Some(state) = part.state.as_mut() {
                        state.status = if is_error { "error" } else { "completed" }.into();
                        if is_error {
                            state.error = Some(text.clone());
                        } else {
                            state.output = Some(text.clone());
                        }
                    }
                }
            }
        }
        Some("result") => {
            state.saw_result = true;
            // Resume mints a fresh session id per turn — track the latest.
            if let Some(sid) = event.get("session_id").and_then(Value::as_str) {
                state.native_session_id = Some(sid.to_string());
            }
            // We deliberately do NOT turn `permission_denials` into approve-me
            // cards. Headless has no interactive approval, and of the modes we
            // offer, only Plan produces denials — and those are *expected*
            // (read-only by design). Surfacing an "Allow" that re-ran the turn
            // under bypass would silently defeat plan mode. The model already
            // narrates the block in text; the recourse is approving the plan
            // (the ExitPlanMode card), which leaves plan mode. Auto/Bypass never
            // deny in the first place.
            //
            // A plan pause is NOT a failure: the CLI records the blocked tools
            // in `permission_denials` but still reports `subtype: "success"` /
            // `is_error: false`. So drive the error path off the result status
            // alone — a genuine failure is still surfaced.
            let subtype = event.get("subtype").and_then(Value::as_str).unwrap_or("");
            let is_error = event
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(subtype != "success");
            if is_error {
                state.turn_errored = true;
                let detail = event
                    .get("result")
                    .and_then(Value::as_str)
                    .unwrap_or(subtype)
                    .to_string();
                ctx.push_error(format!("claude: {detail}"));
            }
            report_result_usage(ctx, event);
            return true;
        }
        _ => {}
    }
    false
}

/// Sum the four token buckets of a Claude `usage` object into the context-window
/// occupancy of that request (input + cache read + cache write + output).
/// Returns `None` when the object is absent or carries none of the fields.
fn claude_used_tokens(usage: Option<&Value>) -> Option<u64> {
    let usage = usage?;
    let field = |name: &str| usage.get(name).and_then(Value::as_u64);
    let keys = [
        "input_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
        "output_tokens",
    ];
    if keys.iter().all(|k| field(k).is_none()) {
        return None;
    }
    Some(keys.iter().filter_map(|k| field(k)).sum())
}

/// Fold the terminal `result` event's usage into the meter: `modelUsage` reports
/// the context window (keyed by model id — pick the turn's model, else the
/// largest entry, so subagent models don't win); occupancy comes from the
/// `assistant` usage already captured this turn, else the last
/// `usage.iterations[]` entry, else the top-level `usage` aggregate.
fn report_result_usage(ctx: &mut TurnCtx, event: &Value) {
    let model_usage = event.get("modelUsage").and_then(Value::as_object);
    let entry = model_usage.and_then(|map| {
        map.get(ctx.model.as_deref().unwrap_or_default())
            .or_else(|| {
                map.values().max_by_key(|v| {
                    v.get("inputTokens").and_then(Value::as_u64).unwrap_or(0)
                        + v.get("outputTokens").and_then(Value::as_u64).unwrap_or(0)
                        + v.get("cacheReadInputTokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0)
                        + v.get("cacheCreationInputTokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0)
                })
            })
    });
    let context_window = entry
        .and_then(|e| e.get("contextWindow"))
        .and_then(Value::as_u64);
    // Prefer usage already captured from an `assistant` event; else fall back to
    // the LAST iteration (the aggregate overstates a multi-iteration context).
    let iteration_used = event
        .pointer("/usage/iterations")
        .and_then(Value::as_array)
        .and_then(|it| it.last())
        .and_then(|last| claude_used_tokens(Some(last)));
    let used = ctx
        .context_usage
        .as_ref()
        .map(|u| u.used_tokens)
        .or(iteration_used)
        .or_else(|| claude_used_tokens(event.get("usage")));
    if let Some(used) = used {
        ctx.report_usage(ContextUsage {
            used_tokens: used,
            context_window,
        });
    }
}

/// The reasoning-level → `--effort` value the spawn config carries. Split out so
/// the harness and the host agree on exactly what the child was launched with.
fn spawn_config(ctx: &TurnCtx) -> SpawnConfig {
    SpawnConfig {
        permission_mode: ctx.permission_mode,
        effort: claude_effort(ctx.reasoning_level.as_deref()).map(str::to_string),
        model: ctx.model.clone(),
        // The turn WANTS the bridge iff it's plan mode under a bound `orx up`
        // port; whether the bridge was actually achieved is recorded on the
        // spawned child's config (a failed write leaves it false and the next
        // plan turn respawns). Keeping the wanted value here means a plan turn
        // reconciles against a child that already has the bridge and reuses it.
        bridge_active: ctx.permission_mode == Some(PermissionMode::Plan)
            && ctx.host.up_port().is_some(),
    }
}

async fn run_turn(ctx: &mut TurnCtx) -> Result<()> {
    let project = ctx.project.clone();
    let session_id = ctx.session_id.clone();
    // The modular orx skills land in the harness's session-skills dir, fresh,
    // for this session's agent to auto-load — source of truth is the trait. Run
    // per turn (worktree + skills refresh); the resident child re-reads the
    // playbook only on respawn, same tradeoff as codex (see opencode.rs's
    // playbook-freshness comment).
    let skills_dir = ClaudeCode.session_skills_dir();
    let (repo, playbook) =
        tokio::task::spawn_blocking(move || ensure_playbook(&project, &session_id, skills_dir))
            .await
            .map_err(|e| anyhow!("playbook task failed: {e}"))??;

    let plan_mode = ctx.permission_mode == Some(PermissionMode::Plan);
    // Clear any bridge-card flag a previous aborted turn left behind so it can't
    // suppress this turn's fallback.
    let _ = ctx.host.take_bridge_prompted(&ctx.session_id);
    // Sweep zombie HELD cards (native_id) a crashed/restarted process left
    // unresolved: they can never be answered again, and once this turn makes the
    // session busy one could capture the composer's typed-text routing. End-turn
    // cards are deliberately left alone — they resume via --resume.
    let _ = ctx.host.resolve_stale_prompts(&ctx.session_id, true).await;

    // Ensure the session's resident child, reconciled to this turn's config: a
    // reused child costs nothing, a model-only change retunes live, a launch-flag
    // change (or a crash) respawns with `--resume`.
    let spec = SpawnSpec {
        chat: ctx.host.clone(),
        session_id: ctx.session_id.clone(),
        repo,
        playbook,
        resume: ctx.native_session_id.clone(),
        config: spawn_config(ctx),
    };
    let client = ctx.host.claude.ensure(spec).await?;
    // The child records what bridge state it ACHIEVED — a failed mcp-config write
    // leaves it false even in plan mode. Drive card suppression off that, not the
    // wanted value.
    let bridge_active = client.config().bridge_active;

    // Route events to this turn before sending the message — nothing is missed.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let _route = client.register_turn(tx);
    if let Err(e) = client.send_user_message(&ctx.text).await {
        // The child is unusable (stdin gone). Kill it so the next turn respawns
        // with `--resume` and recovers this session's context.
        ctx.host.claude.kill_session(&ctx.session_id).await;
        return Err(anyhow!(
            "claude stdin write failed: {e}; see {}",
            crate::store::data_dir().join("agent-claude.log").display()
        ));
    }

    let mut state = TurnState {
        bridge_active,
        ..Default::default()
    };
    // Fold events until the turn's `result`. The caller (here) applies the
    // native session id and flushes per event, keeping `apply_event` store-free.
    loop {
        let Some(event) = rx.recv().await else {
            // The route channel closed without a Closed marker — treat as a
            // dropped turn; the next turn respawns via `--resume`.
            break;
        };
        match event {
            TurnEvent::Line(value) => {
                let done = apply_event(ctx, &mut state, &value);
                if let Some(sid) = state.native_session_id.take() {
                    ctx.set_native_session_id(&sid);
                }
                ctx.maybe_flush();
                if done {
                    break;
                }
            }
            TurnEvent::Closed => {
                // Child died mid-turn (EOF on stdout). The next turn respawns via
                // `--resume`; surface the failure like the old exit path did.
                if let Some(sid) = state.native_session_id.take() {
                    ctx.set_native_session_id(&sid);
                }
                ctx.host.claude.kill_session(&ctx.session_id).await;
                let _ = ctx.flush();
                return Err(anyhow!(
                    "claude exited mid-turn; see {}",
                    crate::store::data_dir().join("agent-claude.log").display()
                ));
            }
        }
    }

    // A channel-end without a `result` means the child closed between turns or
    // the turn was dropped — respawn next turn and report the miss.
    if !state.saw_result {
        ctx.host.claude.kill_session(&ctx.session_id).await;
        let _ = ctx.flush();
        return Err(anyhow!(
            "claude ended the turn without a result; see {}",
            crate::store::data_dir().join("agent-claude.log").display()
        ));
    }

    // The model sometimes ends a plan-mode turn with its plan as plain text and
    // no ExitPlanMode call. Headless leaves no way out of plan mode then — only
    // a plan-card answer switches the resume mode, so a chat "yes" would resume
    // still read-only. Synthesize a card from the final text so approval always
    // has a handle. A plan/permission card the bridge surfaced mid-turn counts
    // as "saw a prompt" (e.g. keep-planning continued this same turn); a mid-turn
    // *question* deliberately does not — its answer is no exit recourse, and the
    // turn may still end with a texty plan.
    let saw_prompt = state.saw_prompt || ctx.host.take_bridge_prompted(&ctx.session_id);
    if should_synthesize_plan(plan_mode, saw_prompt, state.turn_errored, &state.last_text) {
        ctx.upsert_part(WirePart::prompt(
            format!("plan-synth-{}", ctx.assistant.id),
            WirePrompt {
                kind: "plan".into(),
                plan: Some(std::mem::take(&mut state.last_text)),
                synthesized: true,
                ..Default::default()
            },
        ));
    }
    let _ = ctx.flush();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_card_synthesized_only_for_cardless_texty_plan_turns() {
        // The one case that needs it: plan mode, no card fired, no error, text.
        assert!(should_synthesize_plan(
            true,
            false,
            false,
            "Here's my plan…"
        ));
        // Not in plan mode → the mode needs no exit.
        assert!(!should_synthesize_plan(false, false, false, "plan text"));
        // A real card (ExitPlanMode or AskUserQuestion) already surfaced.
        assert!(!should_synthesize_plan(true, true, false, "plan text"));
        // Errored turns surface the error, not a phantom approval.
        assert!(!should_synthesize_plan(true, false, true, "plan text"));
        // Nothing to approve.
        assert!(!should_synthesize_plan(true, false, false, "   "));
        assert!(!should_synthesize_plan(true, false, false, ""));
    }

    fn answer(
        approve: bool,
        resume_mode: Option<&str>,
        answers: &[&str],
        note: Option<&str>,
    ) -> PromptAnswer {
        PromptAnswer {
            session_id: "s".into(),
            prompt_id: "p".into(),
            approve,
            resume_mode: resume_mode.map(str::to_string),
            answers: answers.iter().map(|s| s.to_string()).collect(),
            note: note.map(str::to_string),
        }
    }

    #[test]
    fn permission_mode_maps_neutral_ids_to_claude_cli_strings() {
        // The shared wire ids are neutral; claude.rs is where they're spelled
        // back into Claude's own `--permission-mode` vocabulary.
        assert_eq!(claude_permission_mode(Some(PermissionMode::Ask)), "default");
        assert_eq!(
            claude_permission_mode(Some(PermissionMode::AcceptEdits)),
            "acceptEdits"
        );
        assert_eq!(claude_permission_mode(Some(PermissionMode::Plan)), "plan");
        assert_eq!(claude_permission_mode(Some(PermissionMode::Auto)), "auto");
        assert_eq!(
            claude_permission_mode(Some(PermissionMode::Bypass)),
            "bypassPermissions"
        );
        // No mode → Claude's balanced default.
        assert_eq!(claude_permission_mode(None), "auto");
    }

    #[test]
    fn plan_approve_defaults_to_auto_but_honors_chosen_mode() {
        let (text, mode) = synthesize_resume("plan", &answer(true, None, &[], None));
        assert!(text.contains("approved the plan"));
        assert_eq!(mode, Some(PermissionMode::Auto));

        let (_, mode) = synthesize_resume("plan", &answer(true, Some("accept-edits"), &[], None));
        assert_eq!(mode, Some(PermissionMode::AcceptEdits));
    }

    #[test]
    fn plan_keep_planning_stays_in_plan_mode() {
        let (text, mode) = synthesize_resume("plan", &answer(false, None, &[], Some("tweak X")));
        assert!(text.contains("tweak X"));
        assert_eq!(mode, Some(PermissionMode::Plan));
    }

    #[test]
    fn plan_noteless_deny_is_a_rejection() {
        // The strip's Reject: no note → "stop and wait" wording, still plan
        // mode. The bridge deny arm reuses this string verbatim, and the
        // end-turn path short-circuits to ResumeAction::Nothing before ever
        // sending it — this pins the wording the bridge relays.
        let (text, mode) = synthesize_resume("plan", &answer(false, None, &[], None));
        assert!(text.contains("rejected"), "{text}");
        assert!(text.contains("Stop planning"), "{text}");
        assert_eq!(mode, Some(PermissionMode::Plan));
    }

    #[test]
    fn permission_approve_defaults_to_bypass() {
        // Approving a blocked tool must resume under `bypass` — the only mode
        // that actually grants it. `acceptEdits`/`ask` would re-deny a Bash tool
        // and loop the card. (Verified against the real CLI.)
        let (text, mode) = synthesize_resume("permission", &answer(true, None, &[], None));
        assert!(text.contains("approved"));
        assert_eq!(mode, Some(PermissionMode::Bypass));
        // An explicit resume_mode still wins, if a caller sets one.
        let (_, mode) = synthesize_resume("permission", &answer(true, Some("auto"), &[], None));
        assert_eq!(mode, Some(PermissionMode::Auto));
    }

    #[test]
    fn question_feeds_selections_back_with_no_mode_change() {
        let (text, mode) = synthesize_resume("question", &answer(true, None, &["A", "B"], None));
        assert_eq!(text, "A, B");
        assert_eq!(mode, None);
    }

    #[test]
    fn empty_question_yields_empty_text_so_respond_rejects_it() {
        // No selection, no note → empty resume text; `resume_from_prompt` turns
        // this into an error that keeps the card actionable.
        let (text, _) = synthesize_resume("question", &answer(true, None, &[], None));
        assert!(text.trim().is_empty());
    }

    #[test]
    fn effort_accepts_only_claude_tiers() {
        assert_eq!(claude_effort(Some("xhigh")), Some("xhigh"));
        assert_eq!(claude_effort(Some("max")), Some("max"));
        // A Codex-only id like a bare "medium" is fine (shared), but junk is not.
        assert_eq!(claude_effort(Some("ultra")), None);
        assert_eq!(claude_effort(None), None);
    }

    /// Fold a hand-written stream-json transcript through `apply_event` against a
    /// bare `TurnCtx::test_stub()` — the store-free property. Returns the final
    /// state; asserts the fold stops on the `result` event and no earlier.
    fn fold(ctx: &mut TurnCtx, bridge_active: bool, lines: &[&str]) -> TurnState {
        let mut state = TurnState {
            bridge_active,
            ..Default::default()
        };
        for line in lines {
            let event: Value = serde_json::from_str(line).expect("valid stream-json line");
            let done = apply_event(ctx, &mut state, &event);
            assert_eq!(
                done,
                event.get("type").and_then(Value::as_str) == Some("result"),
                "only the result event ends the fold: {line}"
            );
            if done {
                break;
            }
        }
        state
    }

    #[test]
    fn plain_turn_folds_text_thinking_and_tool_lifecycle() {
        let transcript = [
            r#"{"type":"system","subtype":"init","session_id":"sess-abc"}"#,
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"thinking","thinking":"pondering"},{"type":"text","text":"Reading the file."}]}}"#,
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"call_1","name":"Read","input":{"file_path":"/x/y.rs"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"call_1","content":"fn main() {}"}]}}"#,
            r#"{"type":"result","subtype":"success","session_id":"sess-abc","is_error":false}"#,
        ];
        let mut ctx = TurnCtx::test_stub();
        let state = fold(&mut ctx, false, &transcript);

        assert!(state.saw_result);
        assert!(!state.turn_errored);
        assert!(!state.saw_prompt);
        assert_eq!(state.native_session_id.as_deref(), Some("sess-abc"));
        assert_eq!(state.last_text, "Reading the file.");

        let parts = &ctx.assistant.parts;
        // thinking (m1-0), text (m1-1), tool (call_1) — three distinct parts.
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert_eq!(parts[0].kind, "reasoning");
        assert_eq!(parts[0].text.as_deref(), Some("pondering"));
        assert_eq!(parts[1].kind, "text");
        assert_eq!(parts[1].text.as_deref(), Some("Reading the file."));
        // The tool_result completed the tool part in place, with the input
        // normalized (file_path → filePath for the UI summary).
        assert_eq!(parts[2].kind, "tool");
        let tool = parts[2].state.as_ref().unwrap();
        assert_eq!(tool.status, "completed");
        assert_eq!(tool.output.as_deref(), Some("fn main() {}"));
        assert_eq!(tool.input.as_ref().unwrap()["filePath"], "/x/y.rs");
    }

    #[test]
    fn error_result_flags_the_turn_and_pushes_an_error_part() {
        let transcript = [
            r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"boom"}"#,
        ];
        let mut ctx = TurnCtx::test_stub();
        let state = fold(&mut ctx, false, &transcript);
        assert!(state.saw_result);
        assert!(state.turn_errored);
        // The error part carries the CLI's detail, prefixed.
        let err = ctx
            .assistant
            .parts
            .iter()
            .find(|p| p.tool.as_deref() == Some("error"))
            .expect("error part");
        assert_eq!(
            err.state.as_ref().unwrap().error.as_deref(),
            Some("claude: boom")
        );
    }

    #[test]
    fn stream_deltas_paint_parts_and_the_final_event_overwrites_them() {
        // Deltas accumulate under {mid}-{index}; the complete assistant event
        // then upserts the authoritative text over the very same part — one
        // part, no duplicate, final text wins.
        let transcript = [
            r#"{"type":"system","subtype":"init","session_id":"sd1"}"#,
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"m9"}},"parent_tool_use_id":null}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}},"parent_tool_use_id":null}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Riv"}},"parent_tool_use_id":null}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"ers flow."}},"parent_tool_use_id":null}"#,
            r#"{"type":"assistant","message":{"id":"m9","content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"Rivers flow."}]}}"#,
            r#"{"type":"result","subtype":"success","session_id":"sd1","is_error":false}"#,
        ];
        let mut ctx = TurnCtx::test_stub();
        let state = fold(&mut ctx, false, &transcript);
        let parts = &ctx.assistant.parts;
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert_eq!(parts[0].kind, "reasoning");
        assert_eq!(parts[0].text.as_deref(), Some("hmm"));
        assert_eq!(parts[1].kind, "text");
        assert_eq!(parts[1].text.as_deref(), Some("Rivers flow."));
        // The final assistant event still feeds last_text (plan synthesis).
        assert_eq!(state.last_text, "Rivers flow.");
    }

    #[test]
    fn subagent_stream_deltas_are_ignored() {
        // A Task subagent's nested stream carries parent_tool_use_id — its
        // deltas must not paint the main transcript.
        let transcript = [
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"sub"}},"parent_tool_use_id":"toolu_1"}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"nested"}},"parent_tool_use_id":"toolu_1"}"#,
            r#"{"type":"result","subtype":"success","session_id":"s","is_error":false}"#,
        ];
        let mut ctx = TurnCtx::test_stub();
        fold(&mut ctx, false, &transcript);
        assert!(ctx.assistant.parts.is_empty(), "{:?}", ctx.assistant.parts);
    }

    #[test]
    fn result_without_is_error_falls_back_to_subtype() {
        // The CLI can omit `is_error`; a non-"success" subtype must still fail
        // the turn (the `.unwrap_or(subtype != "success")` fallback).
        let transcript = [
            r#"{"type":"system","subtype":"init","session_id":"s2"}"#,
            r#"{"type":"result","subtype":"error_during_execution","result":"boom"}"#,
        ];
        let mut ctx = TurnCtx::test_stub();
        let state = fold(&mut ctx, false, &transcript);
        assert!(state.saw_result);
        assert!(state.turn_errored);
    }

    #[test]
    fn errored_tool_result_flips_the_tool_part_to_error() {
        let transcript = [
            r#"{"type":"system","subtype":"init","session_id":"s3"}"#,
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"call_1","name":"Bash","input":{"command":"false"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"call_1","is_error":true,"content":"exit 1"}]}}"#,
            r#"{"type":"result","subtype":"success","session_id":"s3","is_error":false}"#,
        ];
        let mut ctx = TurnCtx::test_stub();
        let state = fold(&mut ctx, false, &transcript);
        assert!(!state.turn_errored, "a failed tool is not a failed turn");
        let tool = ctx.assistant.parts[0].state.as_ref().unwrap();
        assert_eq!(tool.status, "error");
        assert_eq!(tool.error.as_deref(), Some("exit 1"));
        assert_eq!(tool.output, None);
    }

    #[test]
    fn plan_mode_texty_plan_flags_synthesize() {
        // Plan mode, no ExitPlanMode call — the model just wrote its plan as
        // text. saw_prompt stays false and the text is captured, so the run_turn
        // fallback (should_synthesize_plan) would synthesize a plan card.
        let transcript = [
            r#"{"type":"system","subtype":"init","session_id":"p1"}"#,
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"Here is my plan: step one, step two."}]}}"#,
            r#"{"type":"result","subtype":"success","session_id":"p1","is_error":false}"#,
        ];
        let mut ctx = TurnCtx::test_stub();
        let state = fold(&mut ctx, false, &transcript);
        assert!(!state.saw_prompt);
        assert!(!state.turn_errored);
        assert!(should_synthesize_plan(
            true,
            state.saw_prompt,
            state.turn_errored,
            &state.last_text
        ));

        // An ExitPlanMode call instead sets saw_prompt and suppresses the card.
        let with_card = [
            r#"{"type":"assistant","message":{"id":"m2","content":[{"type":"tool_use","id":"c1","name":"ExitPlanMode","input":{"plan":"do it"}}]}}"#,
            r#"{"type":"result","subtype":"success","session_id":"p1","is_error":false}"#,
        ];
        let mut ctx = TurnCtx::test_stub();
        let state = fold(&mut ctx, false, &with_card);
        assert!(state.saw_prompt);
        assert!(!should_synthesize_plan(
            true,
            state.saw_prompt,
            state.turn_errored,
            &state.last_text
        ));
        // The ExitPlanMode surfaced as a plan prompt card, not a tool row.
        let card = ctx
            .assistant
            .parts
            .iter()
            .find(|p| p.kind == "prompt")
            .unwrap();
        assert_eq!(card.prompt.as_ref().unwrap().kind, "plan");
    }

    #[test]
    fn bridge_active_suppresses_exitplanmode_and_question_rows() {
        // With the bridge on, the CLI relays ExitPlanMode / AskUserQuestion as
        // held bridge cards; their tool_use must render NOTHING (a duplicate row,
        // then a spurious error row when the answer-denial's tool_result lands).
        let transcript = [
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"c1","name":"ExitPlanMode","input":{"plan":"p"}}]}}"#,
            r#"{"type":"assistant","message":{"id":"m2","content":[{"type":"tool_use","id":"c2","name":"AskUserQuestion","input":{"questions":[{"question":"which?","header":"h","options":[]}]}}]}}"#,
            r#"{"type":"assistant","message":{"id":"m3","content":[{"type":"tool_use","id":"c3","name":"Bash","input":{"command":"ls"}}]}}"#,
            r#"{"type":"result","subtype":"success","session_id":"b1","is_error":false}"#,
        ];
        let mut ctx = TurnCtx::test_stub();
        let state = fold(&mut ctx, true, &transcript);
        assert!(state.saw_result);
        // Only the Bash tool part survives; the two bridge-owned calls render
        // nothing, and neither sets saw_prompt (the bridge tracks that itself).
        assert!(!state.saw_prompt);
        assert_eq!(ctx.assistant.parts.len(), 1, "{:?}", ctx.assistant.parts);
        assert_eq!(ctx.assistant.parts[0].tool.as_deref(), Some("Bash"));
    }

    #[test]
    fn assistant_usage_reports_summed_token_count_without_window() {
        let mut ctx = TurnCtx::test_stub();
        let event: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"hi"}],
                 "usage":{"input_tokens":3,"cache_creation_input_tokens":27557,"cache_read_input_tokens":100,"output_tokens":4}}}"#,
        )
        .unwrap();
        apply_event(&mut ctx, &mut TurnState::default(), &event);
        let usage = ctx.context_usage.expect("assistant usage reported");
        assert_eq!(usage.used_tokens, 3 + 27557 + 100 + 4);
        assert_eq!(usage.context_window, None);
    }

    #[test]
    fn result_modelusage_supplies_window_and_keeps_assistant_tokens() {
        // Real shape captured 2026-07-22 from claude 2.1.197.
        let mut ctx = TurnCtx::test_stub();
        ctx.model = Some("claude-haiku-4-5".into());
        let assistant: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"hi"}],
                 "usage":{"input_tokens":3,"cache_creation_input_tokens":27557,"cache_read_input_tokens":0,"output_tokens":4}}}"#,
        )
        .unwrap();
        apply_event(&mut ctx, &mut TurnState::default(), &assistant);
        let result: Value = serde_json::from_str(
            r#"{"type":"result","subtype":"success","num_turns":1,
                "usage":{"input_tokens":3,"cache_creation_input_tokens":27557,"cache_read_input_tokens":0,"output_tokens":4,
                  "iterations":[{"input_tokens":3,"output_tokens":4,"cache_read_input_tokens":0,"cache_creation_input_tokens":27557,"type":"message"}]},
                "modelUsage":{"claude-haiku-4-5":{"inputTokens":3,"outputTokens":4,"cacheReadInputTokens":0,"cacheCreationInputTokens":27557,"costUSD":0.055137,"contextWindow":200000,"maxOutputTokens":32000}}}"#,
        )
        .unwrap();
        let done = apply_event(&mut ctx, &mut TurnState::default(), &result);
        assert!(done);
        let usage = ctx.context_usage.expect("result usage present");
        // The assistant already reported the tokens; result only adds the window.
        assert_eq!(usage.used_tokens, 3 + 27557 + 4);
        assert_eq!(usage.context_window, Some(200000));
    }

    #[test]
    fn subagent_assistant_usage_does_not_touch_the_meter() {
        // A Task subagent's message is a top-level `assistant` event with
        // `parent_tool_use_id` set; its (smaller) usage must NOT overwrite the
        // main session's occupancy.
        let mut ctx = TurnCtx::test_stub();
        let main: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"hi"}],
                 "usage":{"input_tokens":50000,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":100}}}"#,
        )
        .unwrap();
        apply_event(&mut ctx, &mut TurnState::default(), &main);
        let before = ctx.context_usage.clone().expect("main usage reported");
        assert_eq!(before.used_tokens, 50100);

        let subagent: Value = serde_json::from_str(
            r#"{"type":"assistant","parent_tool_use_id":"toolu_1","message":{"id":"m2","content":[{"type":"text","text":"sub"}],
                 "usage":{"input_tokens":10,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":2}}}"#,
        )
        .unwrap();
        apply_event(&mut ctx, &mut TurnState::default(), &subagent);
        assert_eq!(ctx.context_usage, Some(before));
    }

    #[test]
    fn result_falls_back_to_last_iteration_when_no_assistant_usage() {
        let mut ctx = TurnCtx::test_stub();
        ctx.model = Some("claude-haiku-4-5".into());
        let result: Value = serde_json::from_str(
            r#"{"type":"result","subtype":"success",
                "usage":{"input_tokens":9,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":9,
                  "iterations":[
                    {"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},
                    {"input_tokens":40,"output_tokens":2,"cache_read_input_tokens":5,"cache_creation_input_tokens":3}]},
                "modelUsage":{"claude-haiku-4-5":{"inputTokens":40,"outputTokens":2,"cacheReadInputTokens":5,"cacheCreationInputTokens":3,"contextWindow":200000}}}"#,
        )
        .unwrap();
        apply_event(&mut ctx, &mut TurnState::default(), &result);
        let usage = ctx.context_usage.expect("result usage present");
        // Last iteration (40+2+5+3), not the aggregate.
        assert_eq!(usage.used_tokens, 40 + 2 + 5 + 3);
        assert_eq!(usage.context_window, Some(200000));
    }
}
