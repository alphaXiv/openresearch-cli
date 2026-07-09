//! Claude Code harness.
//!
//! Chat: one `claude --print` process per turn, stream-json on stdout,
//! multi-turn via `--resume` against Claude Code's own session store. The
//! playbook rides `--append-system-prompt-file`; permissions are bypassed
//! (parity with the opencode allow-all config — the playbook forbids
//! interactive questions anyway, and headless mode couldn't answer them).
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
        // Only Auto + Bypass. Headless `claude --print` has no interactive
        // approval, so `ask`/`accept-edits` can't grant a blocked tool (they just
        // deny). And `plan` — a hard read-only gate — fights the orx workflow:
        // it blocks the read-only `orx` inspection the agent needs to plan *and*
        // the launches that are the whole point, so "propose then approve"
        // happens better in conversation (the agent describes its plan and asks
        // before running `orx exp run`) than via a mode that can't run orx.
        //   * Auto  — the balanced default; runs tools without prompting.
        //   * Bypass— runs everything, no sandbox.
        HarnessOptions::none()
            .with_permission_modes(
                &[PermissionMode::Auto, PermissionMode::Bypass],
                PermissionMode::Auto,
            )
            // Claude Code's `--effort` tiers (default `high` on current models).
            .with_reasoning_levels(&CLAUDE_EFFORT_LEVELS, "high")
    }

    /// Claude ends its turn on a prompt, so every answer resumes by sending a
    /// *new user message* under `--resume` (see `run_turn`). A denied permission
    /// is the one case with no resume.
    async fn resume_from_prompt(
        &self,
        _ctx: &ResumeCtx,
        prompt: &WirePrompt,
        answer: &PromptAnswer,
    ) -> Result<ResumeAction> {
        // A denied permission closes the card without resuming; every other
        // answer continues the session.
        if prompt.kind == "permission" && !answer.approve {
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
/// default (or the session's mode, applied downstream).
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
            // "Keep planning" — stay in plan mode with the refinement.
            let text = note
                .map(|n| format!("Keep refining the plan: {n}"))
                .unwrap_or_else(|| "Please revise the plan.".to_string());
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
/// we surface the first question (the composer answers one at a time).
fn question_prompt(name: &str, input: Option<&Value>) -> Option<WirePrompt> {
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
    let (repo, playbook) = tokio::task::spawn_blocking(move || ensure_playbook(&project))
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
    // disallowed; the turn ends on them and the answer resumes the session.
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
                            // prompt cards instead of plain tool rows; the CLI
                            // ends the turn on them (headless can't answer inline),
                            // and the user's choice resumes the session.
                            if let Some(prompt) =
                                plan_prompt(name, input).or_else(|| question_prompt(name, input))
                            {
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
