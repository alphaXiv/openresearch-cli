//! Claude Code harness.
//!
//! Chat: one `claude --print` process per turn, stream-json on stdout,
//! multi-turn via `--resume` against Claude Code's own session store. The
//! playbook rides `--append-system-prompt-file`; the permission mode is
//! `--permission-mode` from the session's setting (`auto`/`bypass` — see
//! `options`). AskUserQuestion / ExitPlanMode surface as interactive cards: the
//! turn ends on them and the user's answer resumes the session — except in
//! plan mode, where the mcp-gate bridge holds both open mid-turn and the
//! answer continues the same turn.
//!
//! Detection: `~/.claude.json` carries the signed-in OAuth account (no secrets
//! read); `ANTHROPIC_API_KEY` is the api-key fallback.

use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use super::detect::{bin_version, find_on_path, nonempty_str, read_json, HarnessInfo};
use super::options::{HarnessOptions, PermissionMode};
use super::{Harness, ResumeAction};
use crate::error::{anyhow, Result};
use crate::local::chat::{
    prepare_env, PromptAnswer, ResumeCtx, TurnCtx, WirePart, WirePrompt, WireQuestionOption,
    WireToolState,
};
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
pub fn find_claude() -> Option<PathBuf> {
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
}

/// Session mode → Claude Code `--permission-mode` value. The shared wire ids are
/// harness-agnostic (`ask`/`accept-edits`/`bypass`), so this is where the enum
/// is spelled back into Claude's own CLI vocabulary; `Auto` is the default when
/// the session hasn't picked a mode.
fn claude_permission_mode(mode: Option<PermissionMode>) -> &'static str {
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
fn write_plan_settings(repo: &std::path::Path) -> Result<PathBuf> {
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

/// Write the per-turn `--mcp-config` file pointing Claude at `orx mcp-gate`
/// (this same binary) and return its path. The bridge's env block carries the
/// `orx up` port, the session id, and a fresh per-turn token — everything the
/// child needs to relay permission requests back to the running server.
fn write_mcp_config(
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
fn synthesize_resume(kind: &str, req: &PromptAnswer) -> (String, Option<PermissionMode>) {
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
fn should_synthesize_plan(
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

async fn run_turn(ctx: &mut TurnCtx) -> Result<()> {
    let bin = find_claude().ok_or_else(|| {
        anyhow!("claude not found on PATH — install Claude Code and run `claude` once to sign in")
    })?;
    let project = ctx.project.clone();
    let session_id = ctx.session_id.clone();
    let (repo, playbook) =
        tokio::task::spawn_blocking(move || ensure_playbook(&project, &session_id))
            .await
            .map_err(|e| anyhow!("playbook task failed: {e}"))??;

    let mut cmd = Command::new(&bin);
    cmd.args([
        "--print",
        "--output-format",
        "stream-json",
        "--verbose",
        "--permission-mode",
        claude_permission_mode(ctx.permission_mode),
    ])
    // AskUserQuestion and ExitPlanMode are now surfaced to the user as
    // interactive cards (see plan_prompt / question_prompt) instead of being
    // disallowed; the turn ends on them and the answer resumes the session —
    // unless the plan-mode bridge is active, which holds them open mid-turn.
    .arg("--append-system-prompt-file")
    .arg(&playbook)
    .current_dir(&repo)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::from(crate::local::chat::harness_log("claude")?))
    .kill_on_drop(true);
    if let Some(model) = &ctx.model {
        cmd.args(["--model", model]);
    }
    // Reasoning level maps directly to Claude Code's `--effort` flag.
    if let Some(effort) = claude_effort(ctx.reasoning_level.as_deref()) {
        cmd.args(["--effort", effort]);
    }
    if let Some(native_id) = &ctx.native_session_id {
        cmd.args(["--resume", native_id]);
    }
    // In plan mode, two per-turn files change what the CLI gates:
    //  * `--settings` wires the `orx plan-gate` PreToolUse hook — read-only
    //    inspection allowed, ExitPlanMode forced to `ask` (headless would
    //    self-approve it otherwise).
    //  * `--mcp-config` + `--permission-prompt-tool` wire the `orx mcp-gate`
    //    bridge: every permission the CLI would have prompted for interactively
    //    is relayed to `orx up`, which surfaces a card and holds the call open
    //    until the user answers — desktop-style mid-turn approvals.
    // Both are best-effort: without them plan mode degrades to its default
    // gating (denials) rather than failing the turn. On CLI versions without
    // `--permission-prompt-tool` the flag is silently ignored (verified), so no
    // version gate is needed.
    let mut bridge_active = false;
    if ctx.permission_mode == Some(PermissionMode::Plan) {
        match write_plan_settings(&repo) {
            Ok(path) => {
                cmd.arg("--settings").arg(path);
            }
            Err(e) => {
                eprintln!(
                    "orx up: plan-mode settings not written, orx inspection will be gated: {e}"
                );
            }
        }
        // Without a bound port (not under `orx up`) there's no HTTP surface to
        // relay approvals to — skip the bridge.
        if let Some(port) = ctx.host.up_port() {
            let token = ctx.host.mint_gate_token(&ctx.session_id);
            match write_mcp_config(&repo, port, &ctx.session_id, &token) {
                Ok(path) => {
                    cmd.arg("--mcp-config").arg(path);
                    cmd.args(["--permission-prompt-tool", "mcp__orx__approve"]);
                    // Give a held approval an hour before the CLI abandons
                    // the tool call; orx denies at 55 min, safely inside it.
                    cmd.env("MCP_TOOL_TIMEOUT", "3600000");
                    bridge_active = true;
                }
                Err(e) => {
                    eprintln!(
                        "orx up: mcp bridge not configured, gray-area tools will be denied: {e}"
                    );
                }
            }
        }
    }
    prepare_env(&mut cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("Could not spawn {}: {}", bin.display(), e))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(ctx.text.as_bytes()).await?;
        // Dropped here: EOF is what tells --print the prompt is complete.
    }
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
    let mut lines = BufReader::new(stdout).lines();
    let mut saw_result = false;
    // Synthesized-plan-card tracking (see `should_synthesize_plan`): whether
    // any interactive card was surfaced, whether the turn errored, and the
    // last non-empty text block — the plan, if the model wrote one as text.
    // Clear any bridge-card flag a previous aborted turn left behind so it
    // can't suppress this turn's fallback.
    let plan_mode = ctx.permission_mode == Some(PermissionMode::Plan);
    let _ = ctx.host.take_bridge_prompted(&ctx.session_id);
    // Sweep zombie HELD cards (native_id) a crashed/restarted process left
    // unresolved: they can never be answered again, and once this turn makes
    // the session busy one could capture the composer's typed-text routing.
    // End-turn cards are deliberately left alone — they resume via --resume.
    let _ = ctx.host.resolve_stale_prompts(&ctx.session_id, true).await;
    let mut saw_prompt = false;
    let mut turn_errored = false;
    let mut last_text = String::new();

    while let Some(line) = lines.next_line().await? {
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("system") => {
                if event.get("subtype").and_then(Value::as_str) == Some("init") {
                    if let Some(sid) = event.get("session_id").and_then(Value::as_str) {
                        ctx.set_native_session_id(sid);
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
                                last_text = text.to_string();
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
                            // prompt cards instead of plain tool rows, and the
                            // user's choice resumes the session. With the bridge
                            // active, BOTH cards come from the bridge instead
                            // (held, mid-turn-answerable) and the tool_use
                            // renders NOTHING: a tool row would duplicate the
                            // card — and the denial that carries the user's
                            // answer back would paint it as a spurious error
                            // row once the tool_result lands.
                            if bridge_active && matches!(name, "ExitPlanMode" | "AskUserQuestion") {
                                continue;
                            }
                            if let Some(prompt) =
                                plan_prompt(name, input).or_else(|| question_prompt(name, input))
                            {
                                saw_prompt = true;
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
                                });
                            }
                        }
                        _ => {}
                    }
                }
                ctx.maybe_flush();
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
                ctx.maybe_flush();
            }
            Some("result") => {
                saw_result = true;
                // Resume mints a fresh session id per turn — track the latest.
                if let Some(sid) = event.get("session_id").and_then(Value::as_str) {
                    ctx.set_native_session_id(sid);
                }
                // We deliberately do NOT turn `permission_denials` into approve-me
                // cards. Headless has no interactive approval, and of the modes we
                // offer, only Plan produces denials — and those are *expected*
                // (read-only by design). Surfacing an "Allow" that re-ran the turn
                // under bypass would silently defeat plan mode. The model already
                // narrates the block in text; the recourse is approving the plan
                // (the ExitPlanMode card), which leaves plan mode. Auto/Bypass
                // never deny in the first place.
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
                    turn_errored = true;
                    let detail = event
                        .get("result")
                        .and_then(Value::as_str)
                        .unwrap_or(subtype)
                        .to_string();
                    ctx.push_error(format!("claude: {detail}"));
                }
                ctx.maybe_flush();
            }
            _ => {}
        }
    }

    // The model sometimes ends a plan-mode turn with its plan as plain text
    // and no ExitPlanMode call. Headless leaves no way out of plan mode then —
    // only a plan-card answer switches the resume mode, so a chat "yes" would
    // resume still read-only. Synthesize a card from the final text so
    // approval always has a handle. A plan/permission card the bridge surfaced
    // mid-turn counts as "saw a prompt" (e.g. keep-planning continued this
    // same turn); a mid-turn *question* deliberately does not — its answer is
    // no exit recourse, and the turn may still end with a texty plan.
    let saw_prompt = saw_prompt || ctx.host.take_bridge_prompted(&ctx.session_id);
    if should_synthesize_plan(plan_mode, saw_prompt, turn_errored, &last_text) {
        ctx.upsert_part(WirePart::prompt(
            format!("plan-synth-{}", ctx.assistant.id),
            WirePrompt {
                kind: "plan".into(),
                plan: Some(last_text),
                synthesized: true,
                ..Default::default()
            },
        ));
        ctx.maybe_flush();
    }

    let status = child.wait().await?;
    if !status.success() && !saw_result {
        return Err(anyhow!(
            "claude exited with {status}; see {}",
            crate::store::data_dir().join("agent-claude.log").display()
        ));
    }
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
}
