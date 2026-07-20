//! Codex harness.
//!
//! Chat rides Codex's **app-server** protocol (codex ≥ 0.144): one long-lived
//! `codex app-server` child per session (see `local::codex`), a thread per
//! session (`thread/start` / `thread/resume` — the thread id persists as the
//! session's `native_session_id`), one `turn/start` per message, events
//! streamed as JSON-RPC notifications. The playbook rides
//! `developerInstructions` (a real instruction channel — no more first-turn
//! `<system-context>` text wrapping), and the sandbox policy travels per turn
//! (`sandboxPolicy` with writable roots + network). Auto runs
//! `approvalPolicy: on-request`: a command that needs to escalate past the
//! sandbox arrives as a server→client approval request, surfaced as a
//! permission card and answered inline over the same connection
//! (`resume_from_prompt` → `{"decision": accept|decline}`). Verified against
//! codex-cli 0.144.0 via `codex app-server generate-json-schema` plus a live
//! spike; the fixture transcript in the tests pins the wire shapes.
//!
//! Older codex (< 0.144) falls back to the legacy exec path for one release:
//! one `codex exec --json` process per turn, JSONL events on stdout,
//! multi-turn via `codex exec resume <session>`, playbook injected as tagged
//! context on the first turn. `ORX_CODEX_EXEC=1` forces the fallback.
//!
//! Detection: `~/.codex/auth.json` holds either an `OPENAI_API_KEY` or an OAuth
//! `id_token` JWT we decode (unverified) for the account email and plan.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use super::detect::{
    bin_version, find_on_path, jwt_payload, nonempty_str, read_json, resolve_symlinks, title_case,
    HarnessInfo,
};
use super::options::{HarnessOptions, PermissionMode};
use super::{should_synthesize_plan, synthesize_resume, Harness, ResumeAction};
use crate::error::{anyhow, Result};
use crate::local::chat::{
    prepare_env, PromptAnswer, ResumeCtx, TurnCtx, WirePart, WirePrompt, WireQuestionOption,
    WireToolState,
};
use crate::local::codex::{CodexClient, ServerReqKind, TurnEvent};
use crate::local::opencode::ensure_playbook;

// The 5.6 variants (Sol = frontier, Terra = balanced, Luna = fast) plus 5.5;
// ChatGPT-account codex rejects bare `gpt-5.6`. Verified against codex-cli
// 0.144 via `codex exec -m` (5.6 needs >= 0.143; older CLIs get a 400).
const CODEX_MODELS: [&str; 4] = ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna", "gpt-5.5"];

/// Codex's own reasoning vocabulary (id == the `model_reasoning_effort` config
/// value) — the common set across CODEX_MODELS (Sol/Terra also take max/ultra;
/// Luna and 5.5 don't). Reasoning is per-harness (see `options.rs`). Verified
/// against codex-cli 0.144.
const CODEX_REASONING_LEVELS: [(&str, &str); 4] = [
    ("low", "Low"),
    ("medium", "Medium"),
    ("high", "High"),
    ("xhigh", "XHigh"),
];

pub struct Codex;

/// `codex` on PATH, symlinks resolved (see `resolve_symlinks` — codex needs to
/// find its `codex-code-mode-host` helper next to the real binary).
pub fn find_codex() -> Option<PathBuf> {
    find_on_path("codex").map(resolve_symlinks)
}

/// `find_codex` with the install hint baked in (the `find_opencode` precedent)
/// — shared by both transports' spawn paths.
pub(crate) fn find_codex_required() -> Result<PathBuf> {
    find_codex().ok_or_else(|| {
        anyhow!("codex not found on PATH — install Codex and run `codex login` first")
    })
}

#[async_trait]
impl Harness for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn name(&self) -> &'static str {
        "Codex"
    }

    fn supports_chat(&self) -> bool {
        true
    }

    async fn detect(&self) -> Option<HarnessInfo> {
        let mut info = HarnessInfo::new(self.id(), self.name());
        if let Some(bin) = find_codex() {
            info.installed = true;
            info.version = bin_version(&bin).await;
            info.bin_path = Some(bin.to_string_lossy().into_owned());
        }
        if let Some(auth) =
            dirs::home_dir().and_then(|h| read_json(h.join(".codex").join("auth.json")))
        {
            if nonempty_str(&auth, "OPENAI_API_KEY").is_some() {
                info.authenticated = true;
                info.auth_method = Some("apiKey");
            }
            if let Some(claims) = auth
                .get("tokens")
                .and_then(|t| t.get("id_token"))
                .and_then(Value::as_str)
                .and_then(jwt_payload)
            {
                info.authenticated = true;
                info.auth_method = Some("oauth");
                info.account = nonempty_str(&claims, "email");
                if let Some(oa) = claims.get("https://api.openai.com/auth") {
                    info.plan = nonempty_str(oa, "chatgpt_plan_type").map(|p| title_case(&p));
                }
            }
        }

        info.agent_ready = info.installed && info.authenticated;
        if info.agent_ready {
            info = info.with_models(&CODEX_MODELS);
            // Old CLIs still work via the legacy exec path, but miss the
            // app-server wins (permission prompts on sandbox escalations;
            // thread resume).
            let too_old = info
                .version
                .as_deref()
                .and_then(parse_codex_version)
                .is_some_and(|v| v < MIN_APP_SERVER_VERSION);
            if too_old {
                info.agent_note = Some(
                    "This Codex version chats via the legacy exec path — update to 0.144+ for plan mode & permission prompts.".to_string(),
                );
            }
        } else {
            info.agent_note =
                Some("Install Codex and sign in (`codex login`) to chat with it here.".to_string());
        }
        Some(info)
    }

    async fn run_turn(&self, ctx: &mut TurnCtx) -> Result<()> {
        // app-server for codex ≥ 0.144 (the validated protocol version);
        // legacy exec for older CLIs, for one release. ORX_CODEX_EXEC=1 is the
        // escape hatch if app-server misbehaves ("0"/empty don't count).
        let force_exec = std::env::var("ORX_CODEX_EXEC").is_ok_and(|v| !v.is_empty() && v != "0");
        if force_exec || !app_server_supported().await {
            return run_turn_exec(ctx).await;
        }
        run_turn_app_server(ctx).await
    }

    fn options(&self) -> HarnessOptions {
        // Plan + Auto + Bypass over the app-server (codex ≥ 0.144). Plan is a
        // native *collaboration mode*: `turn/start.collaborationMode` injects
        // codex's own plan.md template, enables the `request_user_input` tool,
        // and streams the finished plan as a dedicated `plan` item — the same
        // scheme the codex TUI's `/plan` uses (see `run_turn_app_server`). The
        // legacy exec fallback (< 0.144) has no collaboration mode, so Plan
        // there degrades to a read-only sandbox with no cards (see
        // `codex_sandbox`) — harmless, and noted in the detect-time agent note.
        //   * Plan  — read-mostly planning turn: same sandbox as Auto
        //     (workspace-write + on-request), restricted only by the prompt-level
        //     plan template (see `codex_policies` for the parity gap vs Claude's
        //     hook-gated plan mode).
        //   * Auto  — workspace-write + `on-request` approvals: the writable
        //     roots (orx data dir, hub `.git` — see `sandbox_policy_json`)
        //     plus network keep routine work prompt-free, and anything past
        //     the sandbox surfaces as a permission card. On the exec fallback
        //     approvals stay off (denials fail to the model).
        //   * Bypass— full access, approvals off.
        HarnessOptions::none()
            .with_permission_modes(
                &[
                    PermissionMode::Plan,
                    PermissionMode::Auto,
                    PermissionMode::Bypass,
                ],
                PermissionMode::Auto,
            )
            // Codex's own reasoning tiers via `-c model_reasoning_effort`.
            .with_reasoning_levels(&CODEX_REASONING_LEVELS, "high")
    }

    /// Three prompt kinds resume differently:
    ///
    /// * `permission` (native, held mid-turn): the answer is the JSON-RPC
    ///   `{decision}` reply, delivered inline over the live app-server child —
    ///   the still-running turn keeps streaming once codex unblocks
    ///   ([`ResumeAction::Handled`], never the new-message path).
    /// * `question` (native, held mid-turn): a `request_user_input` reply,
    ///   delivered inline the same way (`user_input_reply`).
    /// * `plan` (end-turn card, no `native_id`): resumes by a NEW user message
    ///   ([`ResumeAction::SendMessage`]) — approve sends the implementation
    ///   prompt under the chosen (default Auto) mode; whose maskless→`default`
    ///   collaborationMode is what actually exits plan mode. Revise stays in
    ///   Plan (shared `synthesize_resume`); a note-less reject just closes the
    ///   card ([`ResumeAction::Nothing`]).
    async fn resume_from_prompt(
        &self,
        ctx: &ResumeCtx,
        prompt: &WirePrompt,
        answer: &PromptAnswer,
    ) -> Result<ResumeAction> {
        match prompt.kind.as_str() {
            // End-turn plan card (no native_id): resume by message, exactly
            // like Claude's plan card. The fresh turn's collaborationMode mask
            // (`default` on approve/leave, `plan` on revise) is what un-sticks
            // or keeps plan mode — no inline reply, so no busy-check here.
            "plan" => {
                // Note-less reject on an end-turn card: the turn is already over
                // and there's nothing to un-stick with a message — resuming just
                // to say "stop" would end in fresh text that becomes ANOTHER
                // plan card, so it could never dismiss the strip. Close it.
                if !answer.approve && answer.note.as_deref().is_none_or(|s| s.trim().is_empty()) {
                    return Ok(ResumeAction::Nothing);
                }
                let note = answer.note.as_deref().filter(|s| !s.trim().is_empty());
                let chosen = answer
                    .resume_mode
                    .as_deref()
                    .and_then(PermissionMode::from_id);
                let (text, mode) = if answer.approve {
                    // Codex's plan template primes the model for "Implement the
                    // plan." — its own proven approval phrasing (the TUI uses
                    // it). Approving leaves plan mode; default to Auto, whose
                    // fresh turn attaches the `default` mask that un-sticks.
                    let mut text = "Implement the plan.".to_string();
                    if let Some(note) = note {
                        text.push_str(&format!("\n\nAdditional guidance: {note}"));
                    }
                    (text, chosen.or(Some(PermissionMode::Auto)))
                } else {
                    // Revise (a note-carrying reject): stay in Plan. Reuse the
                    // shared plan-deny wording so the phrasing matches Claude.
                    synthesize_resume("plan", answer)
                };
                Ok(ResumeAction::SendMessage { text, mode })
            }
            // Native held cards (permission / question): reply inline over the
            // live child. A reply only lands if the turn is still paused on it —
            // after an interrupt/error the request was already settled and a
            // late reply would be a stale answer into the void. Mirror Claude's
            // zombie collapse so a card left by a crashed turn stops swallowing
            // answers.
            "permission" | "question" => {
                if !ctx.is_busy().await {
                    ctx.host
                        .resolve_zombie_prompt(&ctx.session_id, &answer.prompt_id);
                    return Err(anyhow!(
                        "this turn is no longer running — its prompt can't be answered"
                    ));
                }
                let native = prompt
                    .native_id
                    .as_deref()
                    .ok_or_else(|| anyhow!("codex prompt has no reply id"))?;
                // native_id is the JSON-RPC request id's raw text.
                let rpc_id: Value = serde_json::from_str(native)
                    .map_err(|_| anyhow!("codex prompt reply id is invalid"))?;
                // Build the reply BEFORE reaching the client, so a bad answer
                // (a question with no selection/note) errs before delivery and
                // leaves the card actionable.
                let reply = if prompt.kind == "permission" {
                    serde_json::json!({ "decision": approval_decision(answer.approve) })
                } else {
                    user_input_reply(prompt, answer)?
                };
                let client = ctx
                    .host
                    .codex
                    .client_for(&ctx.session_id)
                    .await
                    .ok_or_else(|| {
                        anyhow!("codex app-server is not running — cannot deliver the reply")
                    })?;
                client.respond(&rpc_id, reply).await?;
                Ok(ResumeAction::Handled)
            }
            other => Err(anyhow!("codex cannot reply to a `{other}` prompt")),
        }
    }

    fn config_home(&self) -> Option<PathBuf> {
        Some(dirs::home_dir()?.join(".codex"))
    }

    fn skill_target(&self) -> Option<PathBuf> {
        // Codex now speaks native SKILL.md skills (`~/.agents/skills/`); the
        // legacy `~/.codex/prompts/` path is deprecated and model-invisible. The
        // primary target is the real skill; the legacy prompt still rides along
        // via `extra_skill_targets` for older codex versions.
        Some(
            dirs::home_dir()?
                .join(".agents")
                .join("skills")
                .join("orx")
                .join("SKILL.md"),
        )
    }

    fn skill_shim(&self) -> Option<&'static str> {
        // Native SKILL.md format, same body as Claude Code / OpenCode / Cursor.
        Some(super::CLAUDE_SKILL)
    }

    fn extra_skill_targets(&self) -> Vec<(PathBuf, &'static str)> {
        // Keep the legacy `/orx` prompt for codex versions that don't yet read
        // `~/.agents/skills/`.
        match dirs::home_dir() {
            Some(home) => vec![(
                home.join(".codex").join("prompts").join("orx.md"),
                super::CODEX_PROMPT,
            )],
            None => Vec::new(),
        }
    }

    fn session_skills_dir(&self) -> Option<&'static str> {
        Some(".agents/skills")
    }
}

// --- app-server path (codex ≥ 0.144) -----------------------------------------

/// First protocol version the harness was validated against (schema dump +
/// live spike). Older CLIs take the exec fallback below.
const MIN_APP_SERVER_VERSION: (u64, u64, u64) = (0, 144, 0);

/// A turn with NO events for this long is treated as wedged and interrupted
/// rather than held busy forever. Known false positive: a command that is
/// legitimately silent this long (a quiet build, a training step with
/// buffered output) is indistinguishable from a hang — hence the generous
/// bound; the interruption is a clear, recoverable error either way.
const TURN_WATCHDOG: Duration = Duration::from_secs(30 * 60);

/// `codex --version` output → (major, minor, patch). The first token that
/// parses wins, so "codex-cli 0.144.0", bare "0.144.0", and a future
/// "codex-cli 0.150.0 (abc123)" all resolve; a `-suffix` on the patch is
/// tolerated.
fn parse_codex_version(version: &str) -> Option<(u64, u64, u64)> {
    version.split_whitespace().find_map(|token| {
        let mut parts = token.splitn(3, '.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts
            .next()?
            .split(|c: char| !c.is_ascii_digit())
            .next()?
            .parse()
            .ok()?;
        Some((major, minor, patch))
    })
}

/// Whether the installed codex speaks the validated app-server protocol.
/// Probed once per process (a codex upgrade mid-run takes an `orx up` restart
/// to notice — acceptable).
async fn app_server_supported() -> bool {
    static SUPPORTED: tokio::sync::OnceCell<bool> = tokio::sync::OnceCell::const_new();
    *SUPPORTED
        .get_or_init(|| async {
            let Some(bin) = find_codex() else {
                return false;
            };
            bin_version(&bin)
                .await
                .as_deref()
                .and_then(parse_codex_version)
                .is_some_and(|v| v >= MIN_APP_SERVER_VERSION)
        })
        .await
}

/// Session mode → (thread `sandbox` mode, `approvalPolicy`). Auto runs
/// `on-request`: the sandbox is still the boundary for routine work (the
/// writable roots keep orx traffic prompt-free), and anything that needs to
/// escalate past it arrives as an approval request we surface as a permission
/// card (verified live: 0.144 asks *before* running an out-of-sandbox
/// command). Bypass drops the sandbox, so there is nothing to escalate —
/// approvals stay off.
fn codex_policies(mode: Option<PermissionMode>) -> (&'static str, &'static str) {
    match mode.unwrap_or(PermissionMode::Auto) {
        PermissionMode::Bypass => ("danger-full-access", "never"),
        // Plan runs the SAME sandbox as Auto (workspace-write + on-request).
        // Native plan mode restricts *at the prompt level* — codex's built-in
        // plan.md template tells the model to propose without editing — not at
        // the sandbox level, so this is the parity gap vs Claude's hook-gated
        // plan mode: an off-script write inside the workspace would not prompt
        // (user-accepted). This arm is the variation point if we ever want a
        // harder read-only floor: swap to `("read-only", "on-request")` here
        // and nowhere else. AcceptEdits/Ask still collapse to the balanced
        // default (mirrors `codex_sandbox` on the exec path).
        PermissionMode::Plan => ("workspace-write", "on-request"),
        _ => ("workspace-write", "on-request"),
    }
}

/// The per-turn `sandboxPolicy` object. workspace-write carries the same
/// grants the exec path passed via `-c`: the orx data dir + the hub clone's
/// `.git` as writable roots (see `ensure_orx_data_dir` / `shared_git_dir`),
/// and network on (the agent's job is driving the orx API and git). Like the
/// exec `-c` override, this is a full policy replacement for the turn — a
/// user's own config.toml `sandbox_workspace_write.writable_roots` don't
/// survive it (no append form exists on either transport).
async fn sandbox_policy_json(mode: Option<PermissionMode>, workspace: &Path) -> Value {
    match mode.unwrap_or(PermissionMode::Auto) {
        PermissionMode::Bypass => serde_json::json!({ "type": "dangerFullAccess" }),
        _ => {
            let mut roots: Vec<String> = Vec::new();
            roots.extend(ensure_orx_data_dir().map(|p| p.to_string_lossy().into_owned()));
            roots.extend(
                shared_git_dir(workspace)
                    .await
                    .map(|p| p.to_string_lossy().into_owned()),
            );
            serde_json::json!({
                "type": "workspaceWrite",
                "writableRoots": roots,
                "networkAccess": true,
            })
        }
    }
}

/// The per-turn `collaborationMode` mask (experimental API). Codex's native
/// plan mode is a *collaboration mode*, not a sandbox setting: `plan` injects
/// codex's built-in plan.md template and enables `request_user_input`; `default`
/// injects the Default template. Attaching a mask is never free — even
/// `{mode:"default"}` on a fresh (template-less) thread INJECTS the Default
/// template (verified in the 0.144 spike) — so the caller attaches this only
/// when it actually wants a template (see `run_turn_app_server`).
///
/// Envelope keys are camelCase (`collaborationMode`), `settings` keys snake_case
/// (`reasoning_effort`, `developer_instructions`). `model` is REQUIRED. The
/// built-in template rides `developer_instructions: null`; it's an independent
/// channel from the thread-level `developerInstructions` playbook, so the
/// playbook is never disturbed by leaving this null.
fn collaboration_mode_json(mode: &str, model: &str, effort: Option<&str>) -> Value {
    let mut settings = serde_json::Map::new();
    settings.insert("model".to_string(), Value::String(model.to_string()));
    if let Some(effort) = effort {
        settings.insert(
            "reasoning_effort".to_string(),
            Value::String(effort.to_string()),
        );
    }
    settings.insert("developer_instructions".to_string(), Value::Null);
    serde_json::json!({ "mode": mode, "settings": Value::Object(settings) })
}

/// How a turn ended, from `turn/completed`.
enum TurnEnd {
    /// Completed or interrupted. `interrupted` drives whether an end-turn plan
    /// card is synthesized — an interrupted plan turn has no finished plan.
    Done {
        interrupted: bool,
    },
    Failed(String),
}

/// One app-server notification → transcript state. Pure (fixture-tested):
/// touches only `ctx.assistant.parts` via the TurnCtx helpers. Returns the
/// turn's terminal state when this event ends it.
fn apply_notification(ctx: &mut TurnCtx, method: &str, params: &Value) -> Option<TurnEnd> {
    match method {
        "item/started" | "item/completed" => {
            if let Some(item) = params.get("item") {
                apply_item(ctx, item, method == "item/completed");
            }
        }
        "item/agentMessage/delta" => {
            append_delta(ctx, params, |id| WirePart::text(id, ""));
        }
        // GPT-5 reasoning streams summaries; raw content deltas are the
        // fallback shape. Only one of the two fires per item in practice.
        "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
            append_delta(ctx, params, |id| WirePart::reasoning(id, ""));
        }
        // Plan mode streams the finished plan token-by-token before the
        // completed `plan` item lands. Rendered as a plain markdown text part
        // (WirePart kinds are text|reasoning|tool|prompt) under a derived id so
        // the completed item upserts the same part. The end-turn plan card then
        // reads this part's text as the authoritative plan.
        "item/plan/delta" => {
            let plan_delta = |id: String| WirePart::text(id, "");
            if let Some(item_id) = params.get("itemId").and_then(Value::as_str) {
                let part_id = plan_part_id(item_id);
                if !part_exists(ctx, &part_id) {
                    ctx.upsert_part(plan_delta(part_id.clone()));
                }
                if let Some(delta) = params.get("delta").and_then(Value::as_str) {
                    ctx.append_part_text(&part_id, delta);
                }
            }
        }
        "item/commandExecution/outputDelta" => {
            let (Some(item_id), Some(delta)) = (
                params.get("itemId").and_then(Value::as_str),
                params.get("delta").and_then(Value::as_str),
            ) else {
                return None;
            };
            // Deltas can beat `item/started`; a placeholder part (command
            // unknown yet) is corrected by the later item events.
            if !part_exists(ctx, item_id) {
                ctx.upsert_part(tool_part(
                    item_id.to_string(),
                    "bash",
                    "running",
                    Some(serde_json::json!({ "command": "" })),
                    None,
                ));
            }
            if let Some(part) = ctx.assistant.parts.iter_mut().find(|p| p.id == item_id) {
                if let Some(state) = part.state.as_mut() {
                    let output = state.output.get_or_insert_with(String::new);
                    output.push_str(delta);
                }
            }
        }
        "error" => {
            // Transient errors are retried by codex itself (willRetry); only
            // terminal ones reach the transcript.
            let will_retry = params
                .get("willRetry")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !will_retry {
                ctx.push_error(error_message(params.get("error")));
            }
        }
        "turn/completed" => {
            let turn = params.get("turn").unwrap_or(&Value::Null);
            let status = turn.get("status").and_then(Value::as_str).unwrap_or("");
            if status == "failed" {
                return Some(TurnEnd::Failed(error_message(turn.get("error"))));
            }
            // Defensive: the pins say turn/completed carries a final status,
            // but a non-final one must not truncate the turn if codex ever
            // regresses.
            if status == "inProgress" {
                return None;
            }
            return Some(TurnEnd::Done {
                interrupted: status == "interrupted",
            });
        }
        _ => {}
    }
    None
}

/// Append a streamed delta to its part, creating the (empty) part on the
/// first delta — deltas can arrive before we see `item/started`.
fn append_delta(ctx: &mut TurnCtx, params: &Value, make: impl FnOnce(String) -> WirePart) {
    let (Some(item_id), Some(delta)) = (
        params.get("itemId").and_then(Value::as_str),
        params.get("delta").and_then(Value::as_str),
    ) else {
        return;
    };
    if !part_exists(ctx, item_id) {
        ctx.upsert_part(make(item_id.to_string()));
    }
    ctx.append_part_text(item_id, delta);
}

/// Whether the assistant message already carries a part with this id.
fn part_exists(ctx: &TurnCtx, id: &str) -> bool {
    ctx.assistant.parts.iter().any(|p| p.id == id)
}

/// A tool-flavored WirePart (bash / edit) in one of the three statuses.
fn tool_part(
    id: String,
    tool: &str,
    status: &str,
    input: Option<Value>,
    output: Option<String>,
) -> WirePart {
    WirePart {
        id,
        kind: "tool".into(),
        text: None,
        tool: Some(tool.into()),
        state: Some(WireToolState {
            status: status.into(),
            input,
            output,
            error: None,
            title: None,
        }),
        prompt: None,
    }
}

/// running / error / completed for a (possibly still-open) tool item.
fn tool_status(completed: bool, failed: bool) -> &'static str {
    if !completed {
        "running"
    } else if failed {
        "error"
    } else {
        "completed"
    }
}

/// A ThreadItem (from `item/started` / `item/completed`) → WirePart.
fn apply_item(ctx: &mut TurnCtx, item: &Value, completed: bool) {
    let Some(id) = item.get("id").and_then(Value::as_str).map(str::to_string) else {
        return;
    };
    match item.get("type").and_then(Value::as_str) {
        Some("agentMessage") => {
            let text = item.get("text").and_then(Value::as_str).unwrap_or("");
            // The completed item is authoritative — but never wipe streamed
            // deltas with an empty final text.
            if !completed || !text.is_empty() || !part_exists(ctx, &id) {
                ctx.upsert_part(WirePart::text(id, text));
            }
        }
        Some("reasoning") => {
            let text = reasoning_text(item);
            if !completed || !text.is_empty() || !part_exists(ctx, &id) {
                ctx.upsert_part(WirePart::reasoning(id, &text));
            }
        }
        Some("commandExecution") => {
            let failed = completed
                && (!matches!(
                    item.get("status").and_then(Value::as_str),
                    Some("completed")
                ) || item
                    .get("exitCode")
                    .and_then(Value::as_i64)
                    .is_some_and(|c| c != 0));
            // Streamed output (outputDelta) survives a completed item without
            // aggregatedOutput; when present, aggregatedOutput is authoritative.
            let output = item
                .get("aggregatedOutput")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    ctx.assistant
                        .parts
                        .iter()
                        .find(|p| p.id == id)
                        .and_then(|p| p.state.as_ref())
                        .and_then(|s| s.output.clone())
                });
            let input = serde_json::json!({
                "command": item.get("command").map(command_string).unwrap_or_default(),
            });
            ctx.upsert_part(tool_part(
                id,
                "bash",
                tool_status(completed, failed),
                Some(input),
                output,
            ));
        }
        Some("fileChange") => {
            let failed = completed
                && !matches!(
                    item.get("status").and_then(Value::as_str),
                    Some("completed")
                );
            let input = item
                .get("changes")
                .cloned()
                .map(|c| serde_json::json!({ "changes": c }));
            ctx.upsert_part(tool_part(
                id,
                "edit",
                tool_status(completed, failed),
                input,
                None,
            ));
        }
        Some("plan") => {
            // The completed plan item's `text` is authoritative — but never
            // wipe streamed `item/plan/delta` text with an empty final (same
            // guard as agentMessage). Keyed on the derived plan part id so the
            // completed item upserts the part the deltas built.
            let text = item.get("text").and_then(Value::as_str).unwrap_or("");
            let part_id = plan_part_id(&id);
            if !completed || !text.is_empty() || !part_exists(ctx, &part_id) {
                ctx.upsert_part(WirePart::text(part_id, text));
            }
        }
        // userMessage (our own echo), mcpToolCall, webSearch, …: not rendered
        // (parity with the exec path); unknown types tolerated.
        _ => {}
    }
}

/// The WirePart id of a plan item's text — a pure function of the plan item id
/// so the streamed `item/plan/delta` parts and the completed `plan` item upsert
/// the same part, and `plan_card` can find the authoritative plan text.
fn plan_part_id(item_id: &str) -> String {
    format!("plan-item-{item_id}")
}

/// Display text for a reasoning item: streamed content, else the summary.
fn reasoning_text(item: &Value) -> String {
    let join = |key: &str| {
        item.get(key)
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("\n\n")
            })
            .unwrap_or_default()
    };
    let content = join("content");
    if content.is_empty() {
        join("summary")
    } else {
        content
    }
}

/// Best human-readable message out of a TurnError-ish value.
fn error_message(error: Option<&Value>) -> String {
    error
        .and_then(|e| {
            e.get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| e.as_str().map(str::to_string))
        })
        .unwrap_or_else(|| "codex reported an error".to_string())
}

async fn run_turn_app_server(ctx: &mut TurnCtx) -> Result<()> {
    // Entry sweep: any HELD (native_id) card still unresolved from an earlier
    // turn is a zombie — its JSON-RPC request died with its turn (or child), and
    // worse, codex request ids restart per child, so a click on a stale card
    // could be delivered to a live request minted later. Resolve them before
    // this turn can surface anything. Native-only (`true`) now that end-turn
    // cards exist (the synthesized plan card): those carry no native_id and
    // resume by message — the next user message replaces them, exactly like
    // Claude's precedent. Behavior-preserving for the pre-plan-mode cards (all
    // of which were native).
    ctx.host
        .resolve_stale_prompts(&ctx.session_id, true)
        .await?;
    let project = ctx.project.clone();
    let session_id = ctx.session_id.clone();
    // The modular orx skills land in the harness's session-skills dir, fresh,
    // for this session's agent to auto-load — source of truth is the trait.
    let skills_dir = Codex.session_skills_dir();
    let (repo, playbook) =
        tokio::task::spawn_blocking(move || ensure_playbook(&project, &session_id, skills_dir))
            .await
            .map_err(|e| anyhow!("playbook task failed: {e}"))??;
    let playbook_md = std::fs::read_to_string(&playbook).unwrap_or_default();

    let client = ctx.host.codex.ensure(&ctx.session_id).await?;
    let (sandbox_mode, approval_policy) = codex_policies(ctx.permission_mode);

    // Thread bring-up: reuse the thread this child already carries, resume a
    // persisted one on a fresh child (after an orx up restart or child crash),
    // else start a new thread. The playbook rides developerInstructions on
    // both start and resume, so a long-lived session picks up playbook
    // improvements on the next restart rather than keeping its first version
    // forever.
    let mut thread_setup = serde_json::json!({
        "cwd": repo.to_string_lossy(),
        "sandbox": sandbox_mode,
        "approvalPolicy": approval_policy,
        "developerInstructions": playbook_md,
    });
    if let Some(model) = &ctx.model {
        thread_setup["model"] = Value::String(model.clone());
    }
    let thread_id = match ctx.native_session_id.clone() {
        Some(id) if client.resumed_thread().as_deref() == Some(id.as_str()) => id,
        Some(id) => {
            let mut params = thread_setup.clone();
            params["threadId"] = Value::String(id.clone());
            match client.try_request("thread/resume", params).await? {
                Ok(resumed) => {
                    // Capture the effective model codex reports (top-level
                    // `model`) — the required `settings.model` for a
                    // collaborationMode mask, and the escape path when the
                    // session carries no explicit model.
                    client.set_thread_model(resumed.get("model").and_then(Value::as_str));
                    client.set_resumed_thread(&id);
                    id
                }
                // Codex *rejected* the id (e.g. minted by the old exec path,
                // or the rollout is gone): start a fresh thread; prior context
                // is lost either way. A transport failure, by contrast,
                // propagates as the turn's error (the `?` above) — a resumable
                // thread must never be discarded over a timeout/hiccup.
                Err(err) => {
                    eprintln!(
                        "orx up: codex thread/resume rejected ({err}); starting a fresh thread"
                    );
                    start_thread(ctx, &client, thread_setup).await?
                }
            }
        }
        None => start_thread(ctx, &client, thread_setup).await?,
    };

    // Route events to this turn before starting it — nothing is missed.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let _route = client.register_turn(tx);

    let mut turn_params = serde_json::json!({
        "threadId": thread_id,
        "input": [{ "type": "text", "text": ctx.text }],
        // Explicit per turn — the composer can change mode/model mid-session,
        // and `sandboxPolicy` is the only carrier of writable roots.
        "approvalPolicy": approval_policy,
        "sandboxPolicy": sandbox_policy_json(ctx.permission_mode, &repo).await,
    });
    if let Some(model) = &ctx.model {
        turn_params["model"] = Value::String(model.clone());
    }
    let effort = codex_reasoning(ctx.reasoning_level.as_deref());
    if let Some(effort) = effort {
        turn_params["effort"] = Value::String(effort.to_string());
    }

    // Conditional collaborationMode mask (see `collaboration_mode_json`).
    // Attaching a mask always injects a template, so attach one ONLY when we
    // want it:
    //   * Plan turn → the `plan` mask (codex's plan.md template + question tool).
    //   * Non-plan turn whose thread MAY be sticky-planned → the `default` mask,
    //     once, to un-stick (a `plan` turn leaves the thread planning until a
    //     turn carries `default`; there is no way back to "no template"). "May
    //     be sticky-planned" fires on either signal: the DB `prev_permission_mode`
    //     (survives restarts) or this child's in-memory `last_collab_mode` (a
    //     `plan` mask we sent and haven't cleared).
    //   * Otherwise → attach nothing (preserves today's template-free context).
    // The mask's required `settings.model` is the session model, falling back to
    // codex's reported thread model; keep the top-level `model`/`effort` above
    // so the None-model escape path still works (mask omitted, plain turn).
    let plan_turn = ctx.permission_mode == Some(PermissionMode::Plan);
    let may_be_sticky = ctx.prev_permission_mode == Some(PermissionMode::Plan)
        || client.last_collab_mode() == Some("plan");
    let mask_mode = if plan_turn {
        Some("plan")
    } else if may_be_sticky {
        Some("default")
    } else {
        None
    };
    if let Some(mode) = mask_mode {
        let collab_model = ctx.model.clone().or_else(|| client.thread_model());
        match collab_model {
            Some(model) => {
                turn_params["collaborationMode"] = collaboration_mode_json(mode, &model, effort);
                client.set_last_collab_mode(mode);
            }
            None if plan_turn => {
                // Plan mode with no known model can't build the mask (settings
                // .model is required) — fail clearly rather than silently run a
                // plain (non-planning) turn the user asked to plan.
                return Err(anyhow!(
                    "codex did not report a model — cannot enter plan mode"
                ));
            }
            None => {
                // Un-stick wanted but no model to build the mask: omit it and
                // log. Degrades to today's behavior (the thread stays planned
                // until a turn carries `default`); rare (a resume before any
                // start/resume reported a model).
                eprintln!("orx up: codex reported no model — skipping the plan-mode un-stick mask");
            }
        }
    }

    let started = client.request("turn/start", turn_params).await?;
    // Everything below is filtered to this turn: an earlier turn of the same
    // session that was orx-side aborted (its native interrupt raced or never
    // fired) can still be streaming into the shared channel, and its tail —
    // fatally, its `turn/completed` — must not leak into this transcript.
    let turn_id = started
        .get("turn")
        .and_then(|t| t.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    // Arm the native interrupt now rather than on `turn/started` — an
    // interrupt landing before that notification would otherwise no-op.
    if let Some(turn_id) = turn_id.as_deref() {
        client.set_active_turn(turn_id);
    }

    // Open request cards surfaced this turn: WirePart id → (JSON-RPC request
    // id, kind). Kind picks the settle shape (a permission `decline` vs a
    // userInput empty-answers) on every exit path. Invariant: no unresolved
    // codex card outlives its turn — every exit path below sweeps them (resolve
    // + settle with codex), so a dead turn can't leave a live-looking card. (A
    // task *abort* skips the sweep, but `ChatHost::interrupt` settles pending
    // requests natively first, and the next turn's entry sweep in this function
    // resolves whatever survived.)
    let mut open_requests: HashMap<String, (Value, ServerReqKind)> = HashMap::new();

    loop {
        // Watchdog (see TURN_WATCHDOG for the false-positive trade-off).
        // Suspended while a card is pending — user think-time is unbounded by
        // design (question think-time too); codex's own ~5-minute approval
        // deadline still applies server-side.
        let event = if open_requests.is_empty() {
            match tokio::time::timeout(TURN_WATCHDOG, rx.recv()).await {
                Ok(event) => event,
                Err(_) => {
                    client.interrupt_active_turn().await;
                    ctx.push_error(format!(
                        "codex produced no output for {} minutes — turn interrupted",
                        TURN_WATCHDOG.as_secs() / 60
                    ));
                    let _ = ctx.flush();
                    return Ok(());
                }
            }
        } else {
            rx.recv().await
        };
        let Some(event) = event else {
            return Err(anyhow!("codex app-server event stream ended mid-turn"));
        };
        match event {
            TurnEvent::Notification { method, params } => {
                if event_turn_mismatch(turn_id.as_deref(), &params) {
                    continue;
                }
                // Codex settled a request itself (its approval deadline hit,
                // or our reply raced this notification): the card must not
                // stay live. Part ids are a pure function of the request id;
                // flushed immediately so the card goes read-only right away.
                if method == "serverRequest/resolved" {
                    if let Some(request_id) = params.get("requestId") {
                        let part_id = request_part_id(turn_id.as_deref(), request_id);
                        if open_requests.remove(&part_id).is_some() {
                            resolve_card(ctx, &part_id);
                            let _ = ctx.flush();
                        }
                    }
                }
                match apply_notification(ctx, &method, &params) {
                    Some(TurnEnd::Done { interrupted }) => {
                        sweep_open_requests(ctx, &client, &mut open_requests).await;
                        // Synthesize the end-turn plan card (Plan mode, not
                        // interrupted). Attach before the final flush so the
                        // PlanStrip appears atomically with the finished turn.
                        if plan_turn && !interrupted {
                            if let Some(part) = plan_card(&ctx.assistant.parts, &ctx.assistant.id) {
                                ctx.upsert_part(part);
                            }
                        }
                        let _ = ctx.flush();
                        return Ok(());
                    }
                    Some(TurnEnd::Failed(message)) => {
                        sweep_open_requests(ctx, &client, &mut open_requests).await;
                        // A terminal `error` notification may have already
                        // pushed this exact message — don't render it twice.
                        if !has_error_part(ctx, &message) {
                            ctx.push_error(message);
                        }
                        let _ = ctx.flush();
                        // The turn *finished* (with an error the transcript
                        // already shows); an Err here would double-report.
                        return Ok(());
                    }
                    None => {}
                }
            }
            TurnEvent::Request { id, method, params } => {
                let kind = crate::local::codex::server_req_kind(&method);
                if event_turn_mismatch(turn_id.as_deref(), &params) {
                    // A stale turn's request (aborted predecessor still
                    // streaming) is settled, never surfaced — with the reply
                    // shape its method can actually parse.
                    settle_request(&client, &id, kind).await;
                } else {
                    match kind {
                        ServerReqKind::Approval => {
                            let card = approval_card(turn_id.as_deref(), &method, &id, &params);
                            if let Some((part_id, part)) = card {
                                if matches!(ctx.permission_mode, Some(PermissionMode::Bypass)) {
                                    // Bypass runs sandbox-less with approvals off;
                                    // if codex asks anyway, the user's chosen mode
                                    // answers for them. (Question cards are never
                                    // auto-answered — only approvals.)
                                    let _ = client
                                        .respond(
                                            &id,
                                            serde_json::json!({
                                                "decision": approval_decision(true)
                                            }),
                                        )
                                        .await;
                                } else {
                                    // Surface the card and keep consuming events —
                                    // codex holds the command; the reply arrives
                                    // via `resume_from_prompt` on the user's click.
                                    open_requests.insert(part_id, (id, kind));
                                    ctx.upsert_part(part);
                                    let _ = ctx.flush();
                                }
                            } else {
                                // Classified Approval but no card (unknown method
                                // variant) — decline rather than block.
                                let _ = client.respond_decline(&id).await;
                            }
                        }
                        ServerReqKind::UserInput => {
                            // request_user_input (plan mode's clarifying question)
                            // → a held question card, answered inline or via the
                            // composer. All-secret questions can't be surfaced
                            // (never store secrets) → answer empty so codex
                            // proceeds without them.
                            match user_input_card(turn_id.as_deref(), &id, &params) {
                                Some((part_id, part)) => {
                                    open_requests.insert(part_id, (id, kind));
                                    ctx.upsert_part(part);
                                    let _ = ctx.flush();
                                }
                                None => {
                                    let _ = client
                                        .respond(&id, serde_json::json!({ "answers": {} }))
                                        .await;
                                }
                            }
                        }
                        ServerReqKind::Other => {
                            // A reply schema we don't speak — fail the call
                            // rather than answer in a shape codex can't parse.
                            let _ = client.respond_method_unsupported(&id).await;
                        }
                    }
                }
            }
            TurnEvent::Closed => {
                // Child gone: nothing to settle with codex; just close cards.
                for part_id in std::mem::take(&mut open_requests).into_keys() {
                    resolve_card(ctx, &part_id);
                }
                return Err(anyhow!(
                    "codex app-server exited mid-turn; see {}",
                    crate::store::data_dir().join("agent-codex.log").display()
                ));
            }
        }
        ctx.maybe_flush();
    }
}

/// Turn-exit sweep half of the no-card-outlives-its-turn invariant: cards the
/// user never answered are resolved in the transcript and settled with codex in
/// the shape their kind requires (approval → decline, userInput → empty
/// answers). The settle is unconditional — `CodexClient::respond`'s pending-set
/// guard is the single arbiter, so an already-answered/settled id no-ops there.
async fn sweep_open_requests(
    ctx: &mut TurnCtx,
    client: &CodexClient,
    open: &mut HashMap<String, (Value, ServerReqKind)>,
) {
    for (part_id, (rpc_id, kind)) in open.drain() {
        resolve_card(ctx, &part_id);
        settle_request(client, &rpc_id, kind).await;
    }
}

/// Settle one server→client request in the reply shape its kind requires, so a
/// request orx is abandoning never leaves codex blocked. Approval → `decline`;
/// UserInput → an empty `{"answers": {}}` (codex proceeds without answers);
/// Other → a JSON-RPC method-not-found error.
async fn settle_request(client: &CodexClient, id: &Value, kind: ServerReqKind) {
    match kind {
        ServerReqKind::Approval => {
            let _ = client.respond_decline(id).await;
        }
        ServerReqKind::UserInput => {
            let _ = client
                .respond(id, serde_json::json!({ "answers": {} }))
                .await;
        }
        ServerReqKind::Other => {
            let _ = client.respond_method_unsupported(id).await;
        }
    }
}

/// True when the notification names a turn that is not ours. Notifications
/// without a turn id (warnings, thread-level events) pass through.
fn event_turn_mismatch(expected: Option<&str>, params: &Value) -> bool {
    let Some(expected) = expected else {
        return false;
    };
    let event_turn = params.get("turnId").and_then(Value::as_str).or_else(|| {
        params
            .get("turn")
            .and_then(|t| t.get("id"))
            .and_then(Value::as_str)
    });
    event_turn.is_some_and(|t| t != expected)
}

/// PromptAnswer.approve → the codex decision string. Per-command `accept`
/// (never `acceptForSession` — a single Allow must not silently widen future
/// commands); `decline` lets the model continue and report the denial.
fn approval_decision(approve: bool) -> &'static str {
    if approve {
        "accept"
    } else {
        "decline"
    }
}

/// The WirePart id of a server-request card (approval OR question), a pure
/// function of (turn, request id) — shared by `approval_card`, `user_input_card`,
/// and the `serverRequest/resolved` reconciliation so none needs a reverse
/// lookup. Turn-scoped because codex request ids restart at 0 per child process:
/// without the scope, a stale card from a previous child generation would
/// collide with a live one. (The turn-entry `resolve_stale_prompts` sweep is the
/// primary defense; this makes ids honest too.)
fn request_part_id(turn: Option<&str>, id: &Value) -> String {
    format!("appr-{}-{id}", turn.unwrap_or("t"))
}

/// A server→client approval request → a permission card. Returns the WirePart
/// id and the part; `native_id` carries the JSON-RPC request id's raw text —
/// the reply target for `resume_from_prompt`. `None` for request methods we
/// don't card (they get a JSON-RPC error reply instead — including
/// `item/permissions/requestApproval`, whose reply is a permission-profile
/// object, not a `{decision}`). The key list spans both carded schemas:
/// command/cwd exist only on commandExecution; fileChange carries just
/// reason/grantRoot, so its card leans on `reason`.
fn approval_card(
    turn: Option<&str>,
    method: &str,
    id: &Value,
    params: &Value,
) -> Option<(String, WirePart)> {
    let tool = match method {
        "item/commandExecution/requestApproval" => "bash",
        "item/fileChange/requestApproval" => "edit",
        _ => return None,
    };
    let mut input = serde_json::Map::new();
    for key in ["command", "cwd", "reason", "grantRoot"] {
        if let Some(v) = params.get(key).filter(|v| !v.is_null()) {
            input.insert(key.to_string(), v.clone());
        }
    }
    let part_id = request_part_id(turn, id);
    let prompt = WirePrompt {
        kind: "permission".into(),
        tool: Some(tool.into()),
        tool_input: Some(Value::Object(input)),
        native_id: Some(id.to_string()),
        ..Default::default()
    };
    Some((part_id.clone(), WirePart::prompt(part_id, prompt)))
}

/// An `item/tool/requestUserInput` server request → a `question` card. Codex's
/// schema is `{questions: [{id, header, question, isOther, isSecret, options:
/// [{label, description}]|null}]}`. We surface the FIRST non-secret question
/// (the composer answers one at a time); `native_id` carries the JSON-RPC id so
/// `resume_from_prompt` can reply. `tool_input` stashes every question id plus
/// the one we surfaced, so `user_input_reply` can fill an empty answer for the
/// rest (codex tolerates a partial `answers` map). `None` when there is no
/// non-secret question to show (all-secret / empty) — the caller answers empty
/// (`{"answers":{}}`) and never stores a secret prompt.
fn user_input_card(turn: Option<&str>, id: &Value, params: &Value) -> Option<(String, WirePart)> {
    let questions = params.get("questions").and_then(Value::as_array)?;
    // Every question id, for the multi-question reply fill.
    let all_ids: Vec<Value> = questions
        .iter()
        .filter_map(|q| q.get("id").cloned())
        .collect();
    // The first non-secret question is the one we surface.
    let q = questions
        .iter()
        .find(|q| !q.get("isSecret").and_then(Value::as_bool).unwrap_or(false))?;
    let answered_id = q.get("id").cloned()?;
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
    let part_id = request_part_id(turn, id);
    let prompt = WirePrompt {
        kind: "question".into(),
        question: q
            .get("question")
            .and_then(Value::as_str)
            .map(str::to_string),
        header: q.get("header").and_then(Value::as_str).map(str::to_string),
        options,
        // codex's request_user_input takes one answer per question id — no
        // multi-select notion, so leave it false.
        multi_select: false,
        native_id: Some(id.to_string()),
        tool_input: Some(serde_json::json!({
            "questionIds": all_ids,
            "answeredId": answered_id,
        })),
        ..Default::default()
    };
    Some((part_id.clone(), WirePart::prompt(part_id, prompt)))
}

/// The `item/tool/requestUserInput` reply for an answered question card: the
/// surfaced question id gets the selected labels (or the freeform note when
/// there's no selection), every other stashed id gets an empty `{"answers": []}`
/// (codex tolerates a partial map and proceeds). `Err` when neither a selection
/// nor a note was provided — leaves the card actionable rather than sending an
/// empty answer the user didn't intend.
fn user_input_reply(prompt: &WirePrompt, answer: &PromptAnswer) -> Result<Value> {
    let note = answer.note.as_deref().filter(|s| !s.trim().is_empty());
    let selected: Vec<String> = if !answer.answers.is_empty() {
        answer.answers.clone()
    } else if let Some(note) = note {
        vec![note.to_string()]
    } else {
        return Err(anyhow!("select an option (or type an answer) to reply"));
    };
    let tool_input = prompt.tool_input.as_ref();
    let answered_id = tool_input
        .and_then(|t| t.get("answeredId"))
        .cloned()
        .ok_or_else(|| anyhow!("codex question card has no answer id"))?;
    let mut answers = serde_json::Map::new();
    answers.insert(
        json_key(&answered_id),
        serde_json::json!({ "answers": selected }),
    );
    // Fill the remaining question ids empty so the whole call is answered.
    if let Some(ids) = tool_input
        .and_then(|t| t.get("questionIds"))
        .and_then(Value::as_array)
    {
        for qid in ids {
            let key = json_key(qid);
            answers
                .entry(key)
                .or_insert_with(|| serde_json::json!({ "answers": [] }));
        }
    }
    Ok(serde_json::json!({ "answers": Value::Object(answers) }))
}

/// A JSON value used as a `{"answers": {...}}` map key — a JSON object key is a
/// string, so a string id is used bare and anything else (a numeric id) by its
/// JSON text.
fn json_key(id: &Value) -> String {
    id.as_str()
        .map(str::to_string)
        .unwrap_or_else(|| id.to_string())
}

/// The end-turn plan card for a finished Plan-mode turn, as a ready-to-upsert
/// `WirePart` (id `plan-synth-{assistant_id}`, exactly like Claude's). Prefers
/// the authoritative plan item text (the `plan-item-*` part built from
/// `item/plan/delta` + the completed `plan` item; `synthesized: false`); falls
/// back to the last non-empty text part gated by the shared
/// `should_synthesize_plan` predicate (`synthesized: true`) — the model
/// presented the plan as prose without emitting a `plan` item. `None` when there
/// is nothing to approve. No `native_id`: an end-turn card resumes by message,
/// exactly like Claude's synthesized plan card.
fn plan_card(parts: &[WirePart], assistant_id: &str) -> Option<WirePart> {
    // Authoritative plan item text, if any streamed/completed this turn.
    let plan_text = parts
        .iter()
        .find(|p| p.id.starts_with("plan-item-"))
        .and_then(|p| p.text.as_deref())
        .filter(|t| !t.trim().is_empty());
    let card = if let Some(text) = plan_text {
        WirePrompt {
            kind: "plan".into(),
            plan: Some(text.to_string()),
            synthesized: false,
            ..Default::default()
        }
    } else {
        // No plan item — fall back to the last non-empty text part, gated by the
        // same predicate Claude uses (plan mode, no prompt surfaced, no error,
        // non-empty text). `saw_prompt = false`: any surfaced question/approval
        // card here doesn't count as an exit recourse (mirrors Claude — only a
        // plan answer exits plan mode), so a texty plan still gets a card.
        let last_text = parts
            .iter()
            .rev()
            .find(|p| p.kind == "text" && p.text.as_deref().is_some_and(|t| !t.trim().is_empty()))
            .and_then(|p| p.text.as_deref())?;
        let errored = parts.iter().any(|p| {
            p.state
                .as_ref()
                .is_some_and(|s| s.status == "error" && s.error.is_some())
        });
        if !should_synthesize_plan(true, false, errored, last_text) {
            return None;
        }
        WirePrompt {
            kind: "plan".into(),
            plan: Some(last_text.to_string()),
            synthesized: true,
            ..Default::default()
        }
    };
    Some(WirePart::prompt(format!("plan-synth-{assistant_id}"), card))
}

/// Mark a surfaced card resolved in the in-memory transcript (no-op when the
/// user already answered it). A card resolved by the user goes through
/// `ChatHost::respond` → store; `adopt_resolved_prompts` keeps the two views
/// consistent on flush.
fn resolve_card(ctx: &mut TurnCtx, part_id: &str) {
    if let Some(part) = ctx.assistant.parts.iter_mut().find(|p| p.id == part_id) {
        if let Some(prompt) = part.prompt.as_mut() {
            prompt.resolved = true;
        }
    }
}

/// Whether the transcript already shows an error part with this message.
fn has_error_part(ctx: &TurnCtx, message: &str) -> bool {
    ctx.assistant.parts.iter().any(|p| {
        p.state
            .as_ref()
            .is_some_and(|s| s.status == "error" && s.error.as_deref() == Some(message))
    })
}

/// `thread/start` and record the new thread id as the session's native id.
async fn start_thread(ctx: &mut TurnCtx, client: &CodexClient, params: Value) -> Result<String> {
    let result = client.request("thread/start", params).await?;
    let thread_id = result
        .get("thread")
        .and_then(|t| t.get("id"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("thread/start returned no thread id"))?
        .to_string();
    // Capture the effective model (top-level `model`) — the required
    // `settings.model` for a collaborationMode mask, and the escape path when
    // the session carries no explicit model.
    client.set_thread_model(result.get("model").and_then(Value::as_str));
    ctx.set_native_session_id(&thread_id);
    client.set_resumed_thread(&thread_id);
    Ok(thread_id)
}

// --- shared by both transports -------------------------------------------

/// The workspace's git dir (`git rev-parse --git-common-dir`), canonicalized.
/// Codex's `workspace-write` sandbox denies every git metadata write: it marks
/// each writable root's `.git` read-only, and a *worktree's* real metadata
/// (refs, objects, `FETCH_HEAD` under `.git/worktrees/<id>/`) lives in the
/// parent clone, outside the workspace entirely — orx session repos are always
/// worktrees of the hub clone. Interactively both denials escalate to approval
/// prompts; `codex exec` has none, so `git fetch`/`commit` just dies with
/// "Operation not permitted". Declaring the common dir as an explicit writable
/// root fixes both shapes — an explicit root beats the built-in `.git`
/// protection (verified against codex-cli 0.144 via `codex sandbox`, plain
/// clone and worktree). Canonicalized because codex requires absolute roots
/// and seatbelt matches real paths (`/var` vs `/private/var`).
async fn shared_git_dir(workspace: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(workspace)
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if line.is_empty() {
        return None;
    }
    absolute_git_dir(workspace, Path::new(&line))
}

/// `dir` as an absolute, symlink-free path; `git rev-parse` answers relative
/// to the workspace for a regular clone (`.git`) and absolute for a worktree.
fn absolute_git_dir(workspace: &Path, dir: &Path) -> Option<PathBuf> {
    workspace.join(dir).canonicalize().ok()
}

/// The orx data dir as a sandbox writable root. The `orx` CLI the agent
/// drives opens the SQLite store read-write (plus journal/WAL sidecars)
/// directly at `store::data_dir()`, which sits under `~/.local/share` —
/// outside every workspace — so `workspace-write` denies the open and every
/// store-touching command dies with "unable to open database file". Created
/// here (host side, unsandboxed) so canonicalize can't fail before first use;
/// canonicalized for the same reason as `shared_git_dir`. Note the grant is
/// the whole data dir — every project's store rows plus `run-logs/` and the
/// `agent-*.log` files — not scoped to the session; that's inherent to the
/// CLI opening the shared DB directly, and still strictly narrower than
/// Bypass.
pub(crate) fn ensure_orx_data_dir() -> Option<PathBuf> {
    let dir = crate::store::data_dir();
    std::fs::create_dir_all(&dir).ok()?;
    dir.canonicalize().ok()
}

/// Session reasoning id → Codex `model_reasoning_effort` value. The composer only
/// offers ids from `CODEX_REASONING_LEVELS`; an unrecognized/absent value omits
/// the override and lets Codex apply its configured default.
fn codex_reasoning(level: Option<&str>) -> Option<&str> {
    let level = level?;
    CODEX_REASONING_LEVELS
        .iter()
        .any(|(id, _)| *id == level)
        .then_some(level)
}

fn command_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

// --- legacy exec path (codex < 0.144, and ORX_CODEX_EXEC=1) -------------------

/// Session mode → Codex `exec` sandbox policy. `codex exec` can't prompt for
/// approval, so the sandbox *is* the permission boundary. `Bypass` is the one
/// mode that also drops the sandbox entirely (`--dangerously-...`); the rest run
/// sandboxed with approvals set to `never` (nothing to escalate to). Returns
/// `None` for `Bypass` to signal "use the bypass flag instead of `-s`".
fn codex_sandbox(mode: Option<PermissionMode>) -> Option<&'static str> {
    match mode.unwrap_or(PermissionMode::Auto) {
        PermissionMode::Plan => Some("read-only"),
        // AcceptEdits/Ask have no distinct exec semantics — treat as the
        // balanced default so a session that carries them still runs sanely.
        PermissionMode::Auto | PermissionMode::AcceptEdits | PermissionMode::Ask => {
            Some("workspace-write")
        }
        PermissionMode::Bypass => None,
    }
}

/// The `-c` value granting `roots` as sandbox writable roots, e.g.
/// `sandbox_workspace_write.writable_roots=["/a", "/b"]`. `None` when there
/// are no roots (omit the flag: `-c ...=[]` would still *replace* the user's
/// configured roots with nothing).
fn writable_roots_override(roots: &[PathBuf]) -> Option<String> {
    if roots.is_empty() {
        return None;
    }
    let list: Vec<String> = roots.iter().map(|p| toml_string(p)).collect();
    Some(format!(
        "sandbox_workspace_write.writable_roots=[{}]",
        list.join(", ")
    ))
}

/// A path as a TOML basic-string literal, for `-c key="value"` overrides.
/// serde_json's escaping emits only sequences TOML also accepts (`\"`, `\\`,
/// control chars as `\uXXXX`) and leaves `/` literal — except DEL (0x7F),
/// which serde_json passes through and TOML forbids unescaped.
fn toml_string(path: &Path) -> String {
    serde_json::to_string(&path.to_string_lossy())
        .unwrap_or_else(|_| String::from("\"\""))
        .replace('\u{7f}', "\\u007F")
}

async fn run_turn_exec(ctx: &mut TurnCtx) -> Result<()> {
    let bin = find_codex_required()?;
    let project = ctx.project.clone();
    let session_id = ctx.session_id.clone();
    // The modular orx skills land in the harness's session-skills dir, fresh,
    // for this session's agent to auto-load — source of truth is the trait.
    let skills_dir = Codex.session_skills_dir();
    let (repo, playbook) =
        tokio::task::spawn_blocking(move || ensure_playbook(&project, &session_id, skills_dir))
            .await
            .map_err(|e| anyhow!("playbook task failed: {e}"))??;

    let mut cmd = Command::new(&bin);
    match &ctx.native_session_id {
        Some(native_id) => {
            cmd.args(["exec", "resume", native_id]);
        }
        None => {
            cmd.arg("exec");
        }
    }
    cmd.args(["--json", "--skip-git-repo-check"])
        .current_dir(&repo)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(crate::local::chat::harness_log("codex")?))
        .kill_on_drop(true);
    // Permission mode → sandbox policy. `codex exec` can't prompt, so the
    // sandbox is the approval boundary: non-bypass modes run sandboxed with
    // approvals disabled (nothing to escalate to), `Bypass` drops both.
    //
    // Set the policy via `-c sandbox_mode=` rather than `-s`: the `exec resume`
    // subcommand rejects `-s` ("unexpected argument"), but accepts `-c` on both
    // the fresh and resume paths (verified against codex-cli 0.143), so one form
    // works for the whole session lifecycle.
    //
    // Yields the data dir granted as a writable root (if any), so the child's
    // store can be pinned to it below, after `prepare_env`.
    let data_dir_pin = match codex_sandbox(ctx.permission_mode) {
        Some(policy) => {
            cmd.args([
                "-c",
                &format!("sandbox_mode=\"{policy}\""),
                "-c",
                "approval_policy=\"never\"",
            ]);
            // workspace-write out of the box is too tight for the orx
            // workflow in three ways (all verified via `codex sandbox` against
            // codex-cli 0.144; in the TUI these denials escalate to approval
            // prompts, which `codex exec` doesn't have):
            //   * Network is blocked by default — DNS doesn't even resolve, so
            //     `git fetch`/`push`, package installs, and the `orx` CLI's
            //     localhost API calls all die. The agent's job is launching
            //     experiments over that API; Auto must keep the network open.
            //   * The orx store isn't writable — the SQLite DB lives in the
            //     data dir under `~/.local/share`, outside the workspace, so
            //     every `orx` command that touches it fails with "unable to
            //     open database file"; grant the data dir (see
            //     `ensure_orx_data_dir`).
            //   * Git metadata isn't writable — codex protects `.git` inside
            //     the workspace, and a worktree's real metadata (the hub
            //     clone's `.git`) sits outside it — so `git fetch`/`commit`
            //     fail outright; grant the common dir (see `shared_git_dir`).
            // Note `-c` *replaces* any `writable_roots` from the user's
            // config.toml for the turn (there is no append form; `exec
            // --add-dir` is unverified on the resume path).
            if policy == "workspace-write" {
                cmd.args(["-c", "sandbox_workspace_write.network_access=true"]);
                let data_dir = ensure_orx_data_dir();
                let roots: Vec<PathBuf> = [data_dir.clone(), shared_git_dir(&repo).await]
                    .into_iter()
                    .flatten()
                    .collect();
                if let Some(override_arg) = writable_roots_override(&roots) {
                    cmd.args(["-c", &override_arg]);
                }
                data_dir
            } else {
                None
            }
        }
        None => {
            cmd.arg("--dangerously-bypass-approvals-and-sandbox");
            None
        }
    };
    // Reasoning level → Codex's own `model_reasoning_effort` config override.
    if let Some(effort) = codex_reasoning(ctx.reasoning_level.as_deref()) {
        cmd.args(["-c", &format!("model_reasoning_effort=\"{effort}\"")]);
    }
    if let Some(model) = &ctx.model {
        cmd.args(["-m", model]);
    }
    let prompt = if ctx.native_session_id.is_none() {
        let playbook_md = std::fs::read_to_string(&playbook).unwrap_or_default();
        format!(
            "<system-context>\n{playbook_md}\n</system-context>\n\n{}",
            ctx.text
        )
    } else {
        ctx.text.clone()
    };
    cmd.arg(prompt);
    prepare_env(&mut cmd);
    // Pin the sandboxed turn's store to the exact path granted above. The
    // grant was resolved from the host's env, but the child could resolve a
    // different data dir — `prepare_env` injects dashboard-synced vars (a
    // synced `ORX_DATA_DIR`/`XDG_DATA_HOME` absent from the host env), and a
    // relative `ORX_DATA_DIR` resolves against the child's cwd, not ours.
    // Must come after `prepare_env`: later `cmd.env` calls win, and the
    // synced-env injection guards on the *process* env, not the cmd's map.
    // (Unsandboxed Bypass has no grant to stay coherent with, so no pin — a
    // synced `ORX_DATA_DIR` still wins there.)
    if let Some(dir) = &data_dir_pin {
        cmd.env("ORX_DATA_DIR", dir);
    }
    // The sandbox blocks the keyring `gh` keeps its token in ("stored token is
    // invalid" from inside the workspace), so resolve it out here and pass it
    // down; both `gh` and its git credential helper prefer these env vars.
    if let Some(token) = crate::local::git::resolve_github_token() {
        cmd.env("GH_TOKEN", &token);
        cmd.env("GITHUB_TOKEN", token);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("Could not spawn {}: {}", bin.display(), e))?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
    let mut lines = BufReader::new(stdout).lines();
    let mut counter = 0usize;
    let mut next_id = |prefix: &str| {
        counter += 1;
        format!("{prefix}-{counter}")
    };
    // Streaming deltas accumulate into one part until the complete event.
    let mut open_text: Option<String> = None;
    let mut open_reasoning: Option<String> = None;

    while let Some(line) = lines.next_line().await? {
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        // Legacy events nest under "msg"; item-style events are flat.
        let msg = event.get("msg").unwrap_or(&event);
        let kind = msg.get("type").and_then(Value::as_str).unwrap_or("");

        // Session/thread id, wherever this version put it.
        for key in ["session_id", "thread_id", "conversation_id"] {
            if let Some(sid) = msg
                .get(key)
                .or_else(|| event.get(key))
                .and_then(Value::as_str)
            {
                ctx.set_native_session_id(sid);
            }
        }

        match kind {
            "agent_message_delta" => {
                let delta = msg.get("delta").and_then(Value::as_str).unwrap_or("");
                let id = open_text.get_or_insert_with(|| next_id("text")).clone();
                if ctx.assistant.parts.iter().all(|p| p.id != id) {
                    ctx.upsert_part(WirePart::text(id.clone(), ""));
                }
                ctx.append_part_text(&id, delta);
            }
            "agent_message" => {
                let text = msg.get("message").and_then(Value::as_str).unwrap_or("");
                let id = open_text.take().unwrap_or_else(|| next_id("text"));
                ctx.upsert_part(WirePart::text(id, text));
            }
            "agent_reasoning_delta" => {
                let delta = msg.get("delta").and_then(Value::as_str).unwrap_or("");
                let id = open_reasoning
                    .get_or_insert_with(|| next_id("think"))
                    .clone();
                if ctx.assistant.parts.iter().all(|p| p.id != id) {
                    ctx.upsert_part(WirePart::reasoning(id.clone(), ""));
                }
                ctx.append_part_text(&id, delta);
            }
            "agent_reasoning" => {
                let text = msg.get("text").and_then(Value::as_str).unwrap_or("");
                let id = open_reasoning.take().unwrap_or_else(|| next_id("think"));
                ctx.upsert_part(WirePart::reasoning(id, text));
            }
            "exec_command_begin" => {
                let id = msg
                    .get("call_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| next_id("cmd"));
                let command = msg.get("command").map(command_string).unwrap_or_default();
                ctx.upsert_part(WirePart {
                    id,
                    kind: "tool".into(),
                    text: None,
                    tool: Some("bash".into()),
                    state: Some(WireToolState {
                        status: "running".into(),
                        input: Some(serde_json::json!({ "command": command })),
                        output: None,
                        error: None,
                        title: None,
                    }),
                    prompt: None,
                });
            }
            "exec_command_end" => {
                let call_id = msg.get("call_id").and_then(Value::as_str).unwrap_or("");
                let exit_ok = msg
                    .get("exit_code")
                    .and_then(Value::as_i64)
                    .is_none_or(|c| c == 0);
                let output = msg
                    .get("aggregated_output")
                    .or_else(|| msg.get("stdout"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if let Some(part) = ctx.assistant.parts.iter_mut().find(|p| p.id == call_id) {
                    if let Some(state) = part.state.as_mut() {
                        state.status = if exit_ok { "completed" } else { "error" }.into();
                        state.output = Some(output);
                    }
                }
            }
            "error" | "stream_error" | "turn.failed" => {
                let detail = msg
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("codex reported an error")
                    .to_string();
                ctx.push_error(detail);
            }
            // Item-style shape: everything interesting is under "item".
            "item.completed" | "item.updated" => {
                if let Some(item) = msg.get("item") {
                    handle_item(ctx, item, &mut next_id);
                }
            }
            _ => {}
        }
        ctx.maybe_flush();
    }

    let status = child.wait().await?;
    if !status.success() {
        return Err(anyhow!(
            "codex exited with {status}; see {}",
            crate::store::data_dir().join("agent-codex.log").display()
        ));
    }
    Ok(())
}

fn handle_item(ctx: &mut TurnCtx, item: &Value, next_id: &mut impl FnMut(&str) -> String) {
    let id = item
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| next_id("item"));
    match item.get("type").and_then(Value::as_str) {
        Some("agent_message") => {
            let text = item.get("text").and_then(Value::as_str).unwrap_or("");
            ctx.upsert_part(WirePart::text(id, text));
        }
        Some("reasoning") => {
            let text = item.get("text").and_then(Value::as_str).unwrap_or("");
            ctx.upsert_part(WirePart::reasoning(id, text));
        }
        Some("command_execution") => {
            let failed = item.get("status").and_then(Value::as_str) == Some("failed")
                || item
                    .get("exit_code")
                    .and_then(Value::as_i64)
                    .is_some_and(|c| c != 0);
            ctx.upsert_part(WirePart {
                id,
                kind: "tool".into(),
                text: None,
                tool: Some("bash".into()),
                state: Some(WireToolState {
                    status: if failed { "error" } else { "completed" }.into(),
                    input: Some(serde_json::json!({
                        "command": item.get("command").map(command_string).unwrap_or_default(),
                    })),
                    output: item
                        .get("aggregated_output")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    error: None,
                    title: None,
                }),
                prompt: None,
            });
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parses_cli_output_and_gates() {
        assert_eq!(parse_codex_version("codex-cli 0.144.0"), Some((0, 144, 0)));
        assert_eq!(parse_codex_version("0.150.2"), Some((0, 150, 2)));
        assert_eq!(
            parse_codex_version("codex-cli 1.0.3-nightly"),
            Some((1, 0, 3))
        );
        assert_eq!(parse_codex_version("codex-cli"), None);
        assert_eq!(parse_codex_version(""), None);
        // The gate itself: tuple ordering does the right thing.
        assert!(parse_codex_version("codex-cli 0.143.9").unwrap() < MIN_APP_SERVER_VERSION);
        assert!(parse_codex_version("codex-cli 0.144.0").unwrap() >= MIN_APP_SERVER_VERSION);
    }

    #[test]
    fn policies_map_modes_to_thread_params() {
        // Every non-bypass mode is the balanced sandbox with on-request
        // approvals (escalations become permission cards); Bypass drops the
        // sandbox, so approvals stay off — nothing to escalate.
        assert_eq!(codex_policies(None), ("workspace-write", "on-request"));
        assert_eq!(
            codex_policies(Some(PermissionMode::Auto)),
            ("workspace-write", "on-request")
        );
        // Plan runs the SAME sandbox as Auto — native plan mode restricts at the
        // prompt level (the plan.md template), not the sandbox level.
        assert_eq!(
            codex_policies(Some(PermissionMode::Plan)),
            ("workspace-write", "on-request")
        );
        assert_eq!(
            codex_policies(Some(PermissionMode::Bypass)),
            ("danger-full-access", "never")
        );
    }

    /// Fold a trimmed live transcript (captured from the 0.144 spike, ids
    /// shortened) through the notification mapper and check the final parts.
    /// Pins: streamed deltas accumulate; the completed agentMessage is
    /// authoritative; a declined/failed command renders as an error tool part;
    /// unknown notifications are ignored; turn/completed ends the fold.
    #[test]
    fn transcript_fold_builds_the_expected_parts() {
        let transcript = [
            r#"{"method":"turn/started","params":{"threadId":"t1","turn":{"id":"turn1","status":"inProgress"}}}"#,
            r#"{"method":"mcpServer/startupStatus/updated","params":{"name":"x","status":"ready"}}"#,
            r#"{"method":"item/started","params":{"item":{"type":"userMessage","id":"u1"},"threadId":"t1","turnId":"turn1"}}"#,
            r#"{"method":"item/started","params":{"item":{"type":"reasoning","id":"rs_1","summary":[],"content":[]},"threadId":"t1","turnId":"turn1"}}"#,
            r#"{"method":"item/reasoning/summaryTextDelta","params":{"delta":"thinking…","itemId":"rs_1","threadId":"t1","turnId":"turn1"}}"#,
            r#"{"method":"item/completed","params":{"item":{"type":"reasoning","id":"rs_1","summary":[],"content":[]},"threadId":"t1","turnId":"turn1"}}"#,
            r#"{"method":"item/started","params":{"item":{"type":"commandExecution","id":"call_1","command":"/bin/zsh -lc 'touch /outside/probe.txt'","cwd":"/ws","status":"inProgress","aggregatedOutput":null,"exitCode":null},"threadId":"t1","turnId":"turn1"}}"#,
            r#"{"method":"item/completed","params":{"item":{"type":"commandExecution","id":"call_1","command":"/bin/zsh -lc 'touch /outside/probe.txt'","cwd":"/ws","status":"declined","aggregatedOutput":null,"exitCode":null},"threadId":"t1","turnId":"turn1"}}"#,
            r#"{"method":"item/started","params":{"item":{"type":"agentMessage","id":"msg_1","text":"","phase":"final_answer"},"threadId":"t1","turnId":"turn1"}}"#,
            r#"{"method":"item/agentMessage/delta","params":{"delta":"Command","itemId":"msg_1","threadId":"t1","turnId":"turn1"}}"#,
            r#"{"method":"item/agentMessage/delta","params":{"delta":" was not run.","itemId":"msg_1","threadId":"t1","turnId":"turn1"}}"#,
            r#"{"method":"item/completed","params":{"item":{"type":"agentMessage","id":"msg_1","text":"Command was not run because the required escalation was rejected.","phase":"final_answer"},"threadId":"t1","turnId":"turn1"}}"#,
            r#"{"method":"turn/completed","params":{"threadId":"t1","turn":{"id":"turn1","status":"completed"}}}"#,
        ];

        let mut ctx = TurnCtx::test_stub();
        let mut ended = None;
        for line in transcript {
            match crate::local::codex::classify_line(line) {
                crate::local::codex::Line::Notification { method, params } => {
                    assert!(
                        !event_turn_mismatch(Some("turn1"), &params),
                        "fixture events all belong to turn1"
                    );
                    if let Some(end) = apply_notification(&mut ctx, &method, &params) {
                        ended = Some(end);
                        break;
                    }
                }
                other => panic!("fixture line classified unexpectedly: {other:?}"),
            }
        }
        assert!(matches!(ended, Some(TurnEnd::Done { interrupted: false })));

        let parts = &ctx.assistant.parts;
        assert_eq!(parts.len(), 3, "reasoning + command + message: {parts:?}");
        // Reasoning: streamed summary delta survives the empty completed item.
        assert_eq!(parts[0].kind, "reasoning");
        assert_eq!(parts[0].text.as_deref(), Some("thinking…"));
        // Declined command → error tool part with the command as input.
        assert_eq!(parts[1].kind, "tool");
        let state = parts[1].state.as_ref().unwrap();
        assert_eq!(state.status, "error");
        assert_eq!(
            state.input.as_ref().unwrap()["command"],
            "/bin/zsh -lc 'touch /outside/probe.txt'"
        );
        // Agent message: the completed item's full text wins over the deltas.
        assert_eq!(parts[2].kind, "text");
        assert_eq!(
            parts[2].text.as_deref(),
            Some("Command was not run because the required escalation was rejected.")
        );
    }

    /// Foreign-turn tails (an aborted predecessor still streaming) are
    /// filtered; turn-less notifications (warnings) pass through.
    #[test]
    fn turn_filter_skips_foreign_turns_only() {
        let expected = Some("turn2");
        assert!(event_turn_mismatch(
            expected,
            &serde_json::json!({"turnId": "turn1", "delta": "stale"})
        ));
        assert!(event_turn_mismatch(
            expected,
            &serde_json::json!({"turn": {"id": "turn1", "status": "completed"}})
        ));
        assert!(!event_turn_mismatch(
            expected,
            &serde_json::json!({"turnId": "turn2"})
        ));
        assert!(!event_turn_mismatch(
            expected,
            &serde_json::json!({"message": "no turn id here"})
        ));
        // Before turn/start answers, nothing is filtered.
        assert!(!event_turn_mismatch(
            None,
            &serde_json::json!({"turnId": "turn1"})
        ));
    }

    #[test]
    fn command_output_deltas_accumulate_and_final_output_wins() {
        let mut ctx = TurnCtx::test_stub();
        apply_notification(
            &mut ctx,
            "item/started",
            &serde_json::json!({"item":{"type":"commandExecution","id":"c1","command":"ls","status":"inProgress"}}),
        );
        for delta in ["a\n", "b\n"] {
            apply_notification(
                &mut ctx,
                "item/commandExecution/outputDelta",
                &serde_json::json!({"itemId":"c1","delta":delta}),
            );
        }
        // No aggregatedOutput on the completed item → streamed output survives.
        apply_notification(
            &mut ctx,
            "item/completed",
            &serde_json::json!({"item":{"type":"commandExecution","id":"c1","command":"ls","status":"completed","exitCode":0}}),
        );
        let state = ctx.assistant.parts[0].state.as_ref().unwrap();
        assert_eq!(state.status, "completed");
        assert_eq!(state.output.as_deref(), Some("a\nb\n"));

        // With aggregatedOutput present, it is authoritative.
        apply_notification(
            &mut ctx,
            "item/completed",
            &serde_json::json!({"item":{"type":"commandExecution","id":"c1","command":"ls","status":"completed","exitCode":0,"aggregatedOutput":"final"}}),
        );
        let state = ctx.assistant.parts[0].state.as_ref().unwrap();
        assert_eq!(state.output.as_deref(), Some("final"));
    }

    /// The live spike's approval request (trimmed) → a permission card whose
    /// native_id round-trips the JSON-RPC id, plus the decision mapping.
    #[test]
    fn approval_request_becomes_a_permission_card() {
        let id = serde_json::json!(0);
        let params = serde_json::json!({
            "threadId": "t1", "turnId": "turn1", "itemId": "call_1",
            "command": "/bin/zsh -lc 'touch /outside/probe.txt'",
            "cwd": "/ws",
            "reason": "Allow writing the requested probe file outside the workspace?",
            "grantRoot": null,
        });
        let (part_id, part) = approval_card(
            Some("turn1"),
            "item/commandExecution/requestApproval",
            &id,
            &params,
        )
        .unwrap();
        assert_eq!(part_id, "appr-turn1-0");
        assert_eq!(part.kind, "prompt");
        let prompt = part.prompt.as_ref().unwrap();
        assert_eq!(prompt.kind, "permission");
        assert_eq!(prompt.tool.as_deref(), Some("bash"));
        assert!(!prompt.resolved);
        // native_id is the raw JSON text of the id — parseable back to Value.
        assert_eq!(prompt.native_id.as_deref(), Some("0"));
        let input = prompt.tool_input.as_ref().unwrap();
        assert_eq!(input["command"], "/bin/zsh -lc 'touch /outside/probe.txt'");
        assert_eq!(input["cwd"], "/ws");
        assert!(input.get("grantRoot").is_none(), "nulls are dropped");

        // fileChange requests carry only reason/grantRoot (no command/cwd) —
        // the edit card leans on `reason`.
        let fc_params = serde_json::json!({
            "threadId": "t1", "turnId": "turn1", "itemId": "fc_1",
            "reason": "Allow writing outside the workspace?",
            "grantRoot": "/outside",
        });
        let (_, part) = approval_card(
            Some("turn1"),
            "item/fileChange/requestApproval",
            &id,
            &fc_params,
        )
        .unwrap();
        let prompt = part.prompt.unwrap();
        assert_eq!(prompt.tool.as_deref(), Some("edit"));
        let input = prompt.tool_input.as_ref().unwrap();
        assert!(input.get("command").is_none());
        assert_eq!(input["reason"], "Allow writing outside the workspace?");
        assert_eq!(input["grantRoot"], "/outside");

        // Non-approval request types → no card (JSON-RPC error reply instead).
        assert!(approval_card(Some("turn1"), "item/tool/requestUserInput", &id, &params).is_none());
        assert!(approval_card(
            Some("turn1"),
            "item/permissions/requestApproval",
            &id,
            &params
        )
        .is_none());

        assert_eq!(approval_decision(true), "accept");
        assert_eq!(approval_decision(false), "decline");
    }

    /// Part ids are turn-scoped: codex request ids restart at 0 per child
    /// process, so the same rpc id in two turns must yield distinct cards.
    #[test]
    fn request_part_ids_are_turn_scoped() {
        let id = serde_json::json!(0);
        assert_eq!(request_part_id(Some("turn1"), &id), "appr-turn1-0");
        assert_ne!(
            request_part_id(Some("turn1"), &id),
            request_part_id(Some("turn2"), &id)
        );
        // No turn id (filter disabled): still deterministic.
        assert_eq!(request_part_id(None, &id), "appr-t-0");
    }

    #[test]
    fn resolve_card_marks_prompts_and_ignores_unknown_parts() {
        let mut ctx = TurnCtx::test_stub();
        let (part_id, part) = approval_card(
            Some("turn1"),
            "item/commandExecution/requestApproval",
            &serde_json::json!(7),
            &serde_json::json!({"command": "x"}),
        )
        .unwrap();
        ctx.upsert_part(part);
        resolve_card(&mut ctx, &part_id);
        assert!(ctx.assistant.parts[0].prompt.as_ref().unwrap().resolved);
        resolve_card(&mut ctx, &part_id); // idempotent
        resolve_card(&mut ctx, "missing"); // no-op, no panic
        assert_eq!(ctx.assistant.parts.len(), 1);
    }

    /// The Failed-dedup guard matches exactly how `push_error` stores errors
    /// (status "error" + the `error` field), and nothing else.
    #[test]
    fn has_error_part_matches_pushed_errors_only() {
        let mut ctx = TurnCtx::test_stub();
        assert!(!has_error_part(&ctx, "boom"));
        ctx.push_error("boom".to_string());
        assert!(has_error_part(&ctx, "boom"));
        assert!(!has_error_part(&ctx, "other"));
        // A failed *command* part is not an error part — its state.error is
        // None, so identical text can't false-match.
        apply_notification(
            &mut ctx,
            "item/completed",
            &serde_json::json!({"item":{"type":"commandExecution","id":"c1","command":"x","status":"failed"}}),
        );
        assert!(!has_error_part(&ctx, "x"));
    }

    #[test]
    fn error_notification_respects_will_retry() {
        let mut ctx = TurnCtx::test_stub();
        apply_notification(
            &mut ctx,
            "error",
            &serde_json::json!({"error":{"message":"transient"},"willRetry":true}),
        );
        assert!(ctx.assistant.parts.is_empty(), "retried errors stay silent");
        apply_notification(
            &mut ctx,
            "error",
            &serde_json::json!({"error":{"message":"fatal"},"willRetry":false}),
        );
        assert_eq!(ctx.assistant.parts.len(), 1);
        let state = ctx.assistant.parts[0].state.as_ref().unwrap();
        assert_eq!(state.status, "error");
    }

    #[test]
    fn failed_turn_surfaces_its_error() {
        let mut ctx = TurnCtx::test_stub();
        let end = apply_notification(
            &mut ctx,
            "turn/completed",
            &serde_json::json!({"turn":{"id":"t","status":"failed","error":{"message":"boom"}}}),
        );
        match end {
            Some(TurnEnd::Failed(msg)) => assert_eq!(msg, "boom"),
            _ => panic!("expected Failed"),
        }
        // Interrupted is a clean end, not a failure — and it carries the flag
        // that suppresses the end-turn plan card.
        let end = apply_notification(
            &mut ctx,
            "turn/completed",
            &serde_json::json!({"turn":{"id":"t","status":"interrupted"}}),
        );
        assert!(matches!(end, Some(TurnEnd::Done { interrupted: true })));
        // A plain completed turn is Done with interrupted:false.
        let end = apply_notification(
            &mut ctx,
            "turn/completed",
            &serde_json::json!({"turn":{"id":"t","status":"completed"}}),
        );
        assert!(matches!(end, Some(TurnEnd::Done { interrupted: false })));
    }

    #[test]
    fn sandbox_maps_modes_to_exec_policies() {
        // Plan is the only read-only mode; the interactive-only modes collapse to
        // the balanced default (exec can't tell them apart); Bypass drops the
        // sandbox (None → the `--dangerously-...` flag).
        assert_eq!(codex_sandbox(Some(PermissionMode::Plan)), Some("read-only"));
        assert_eq!(
            codex_sandbox(Some(PermissionMode::Auto)),
            Some("workspace-write")
        );
        assert_eq!(
            codex_sandbox(Some(PermissionMode::AcceptEdits)),
            Some("workspace-write")
        );
        assert_eq!(
            codex_sandbox(Some(PermissionMode::Ask)),
            Some("workspace-write")
        );
        assert_eq!(codex_sandbox(Some(PermissionMode::Bypass)), None);
        // No mode set → the balanced default, never an accidental full-access.
        assert_eq!(codex_sandbox(None), Some("workspace-write"));
    }

    #[test]
    fn git_dir_resolves_relative_and_absolute_rev_parse_answers() {
        let base = std::env::temp_dir().join(format!("orx-codex-test-{}", std::process::id()));
        let workspace = base.join("worktree");
        let hub_git = base.join("hub").join(".git");
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        std::fs::create_dir_all(&hub_git).unwrap();

        // Worktree: rev-parse answers with the hub clone's absolute path.
        assert_eq!(
            absolute_git_dir(&workspace, &hub_git),
            Some(hub_git.canonicalize().unwrap())
        );
        // Regular clone: rev-parse answers `.git`, relative to the workspace.
        assert_eq!(
            absolute_git_dir(&workspace, Path::new(".git")),
            Some(workspace.join(".git").canonicalize().unwrap())
        );
        // No git dir at all → no writable root (flag omitted, fail-safe).
        assert_eq!(absolute_git_dir(&workspace, Path::new("missing")), None);

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn toml_string_quotes_and_escapes_paths() {
        assert_eq!(
            toml_string(Path::new("/a/with space")),
            r#""/a/with space""#
        );
        assert_eq!(toml_string(Path::new(r#"/a/"q""#)), r#""/a/\"q\"""#);
        // DEL is the one char serde_json leaves raw that TOML rejects.
        assert_eq!(toml_string(Path::new("/a/\u{7f}b")), r#""/a/\u007Fb""#);
    }

    #[test]
    fn writable_roots_override_joins_and_omits_empty() {
        assert_eq!(
            writable_roots_override(&[PathBuf::from("/data dir"), PathBuf::from("/hub/.git")]),
            Some(r#"sandbox_workspace_write.writable_roots=["/data dir", "/hub/.git"]"#.into())
        );
        // No roots → no flag at all; `=[]` would clobber the user's own
        // config.toml roots for the turn.
        assert_eq!(writable_roots_override(&[]), None);
    }

    #[test]
    fn reasoning_accepts_only_codex_ids() {
        assert_eq!(codex_reasoning(Some("low")), Some("low"));
        assert_eq!(codex_reasoning(Some("high")), Some("high"));
        assert_eq!(codex_reasoning(Some("xhigh")), Some("xhigh"));
        // Tiers outside the common set and junk are dropped (the flag is
        // omitted → CLI default), never forwarded as an invalid
        // `model_reasoning_effort`.
        assert_eq!(codex_reasoning(Some("max")), None);
        assert_eq!(codex_reasoning(None), None);
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
    fn collaboration_mode_json_shapes_the_mask() {
        // Envelope key `mode`; settings snake_case; developer_instructions null
        // (independent of the thread-level playbook channel); effort included.
        let plan = collaboration_mode_json("plan", "gpt-5.6-sol", Some("xhigh"));
        assert_eq!(plan["mode"], "plan");
        assert_eq!(plan["settings"]["model"], "gpt-5.6-sol");
        assert_eq!(plan["settings"]["reasoning_effort"], "xhigh");
        assert!(plan["settings"]["developer_instructions"].is_null());

        // Default kind; effort omitted → no `reasoning_effort` key at all.
        let default = collaboration_mode_json("default", "gpt-5.6-sol", None);
        assert_eq!(default["mode"], "default");
        assert_eq!(default["settings"]["model"], "gpt-5.6-sol");
        assert!(default["settings"].get("reasoning_effort").is_none());
        assert!(default["settings"]["developer_instructions"].is_null());
    }

    /// A plan turn: streamed deltas accumulate, the completed `plan` item is
    /// authoritative, and `plan_card` surfaces it as a NON-synthesized card.
    #[test]
    fn plan_deltas_accumulate_and_plan_card_is_authoritative() {
        let mut ctx = TurnCtx::test_stub();
        for delta in ["## Plan\n", "1. do X\n", "2. do Y\n"] {
            apply_notification(
                &mut ctx,
                "item/plan/delta",
                &serde_json::json!({"itemId":"plan_1","delta":delta,"threadId":"t","turnId":"turn1"}),
            );
        }
        // The completed plan item's text is authoritative (upserts the part the
        // deltas built).
        apply_notification(
            &mut ctx,
            "item/completed",
            &serde_json::json!({"item":{"type":"plan","id":"plan_1","text":"## Plan\n1. do X\n2. do Y\n"},"threadId":"t","turnId":"turn1"}),
        );
        let part = ctx
            .assistant
            .parts
            .iter()
            .find(|p| p.id == "plan-item-plan_1")
            .expect("plan part");
        assert_eq!(part.text.as_deref(), Some("## Plan\n1. do X\n2. do Y\n"));

        let card = plan_card(&ctx.assistant.parts, "msgA").expect("plan card");
        assert_eq!(card.id, "plan-synth-msgA");
        let prompt = card.prompt.as_ref().unwrap();
        assert_eq!(prompt.kind, "plan");
        assert!(!prompt.synthesized, "plan item is authoritative");
        assert_eq!(prompt.plan.as_deref(), Some("## Plan\n1. do X\n2. do Y\n"));
        assert!(prompt.native_id.is_none(), "end-turn card has no reply id");
    }

    /// A completed plan item with empty text never wipes the streamed deltas.
    #[test]
    fn empty_completed_plan_item_keeps_streamed_deltas() {
        let mut ctx = TurnCtx::test_stub();
        apply_notification(
            &mut ctx,
            "item/plan/delta",
            &serde_json::json!({"itemId":"plan_1","delta":"streamed plan"}),
        );
        apply_notification(
            &mut ctx,
            "item/completed",
            &serde_json::json!({"item":{"type":"plan","id":"plan_1","text":""}}),
        );
        let part = ctx
            .assistant
            .parts
            .iter()
            .find(|p| p.id == "plan-item-plan_1")
            .unwrap();
        assert_eq!(part.text.as_deref(), Some("streamed plan"));
    }

    /// No plan item, but a texty plan in the final message → a SYNTHESIZED card.
    #[test]
    fn plan_card_falls_back_to_texty_plan() {
        let mut ctx = TurnCtx::test_stub();
        ctx.upsert_part(WirePart::text(
            "msg_1",
            "Here's the plan: step one, step two.",
        ));
        let card = plan_card(&ctx.assistant.parts, "msgA").expect("synthesized card");
        let prompt = card.prompt.as_ref().unwrap();
        assert_eq!(prompt.kind, "plan");
        assert!(prompt.synthesized, "no plan item → synthesized from text");
        assert_eq!(
            prompt.plan.as_deref(),
            Some("Here's the plan: step one, step two.")
        );

        // Nothing to card → None (empty transcript, or only whitespace text).
        assert!(plan_card(&[], "msgA").is_none());
        let mut blank = TurnCtx::test_stub();
        blank.upsert_part(WirePart::text("msg_1", "   "));
        assert!(plan_card(&blank.assistant.parts, "msgA").is_none());
    }

    /// An errored plan turn's transcript → no synthesized card (the error is the
    /// surface, not a phantom approval). An authoritative plan item still cards.
    #[test]
    fn plan_card_suppressed_on_error_unless_plan_item_present() {
        let mut ctx = TurnCtx::test_stub();
        ctx.upsert_part(WirePart::text("msg_1", "partial plan"));
        ctx.push_error("boom".to_string());
        assert!(
            plan_card(&ctx.assistant.parts, "msgA").is_none(),
            "texty plan under an error is not carded"
        );
        // A real plan item is authoritative regardless of an error part.
        ctx.upsert_part(WirePart::text("plan-item-p1", "the plan"));
        let card = plan_card(&ctx.assistant.parts, "msgA").expect("plan item cards");
        assert!(!card.prompt.as_ref().unwrap().synthesized);
    }

    /// requestUserInput → a question card: the first non-secret question is
    /// surfaced, every question id is stashed for the multi-fill reply.
    #[test]
    fn user_input_card_surfaces_first_nonsecret_question() {
        let id = serde_json::json!(3);
        let params = serde_json::json!({
            "threadId":"t","turnId":"turn1","itemId":"call_1",
            "questions":[
                {"id":"q1","header":"Color","question":"Which color?","isOther":false,"isSecret":false,
                 "options":[{"label":"red","description":"warm"},{"label":"blue","description":null}]},
                {"id":"q2","header":"Size","question":"Which size?","isOther":false,"isSecret":false,"options":null},
            ],
        });
        let (part_id, part) = user_input_card(Some("turn1"), &id, &params).expect("card");
        assert_eq!(part_id, "appr-turn1-3");
        let prompt = part.prompt.as_ref().unwrap();
        assert_eq!(prompt.kind, "question");
        assert_eq!(prompt.header.as_deref(), Some("Color"));
        assert_eq!(prompt.question.as_deref(), Some("Which color?"));
        assert_eq!(prompt.native_id.as_deref(), Some("3"));
        assert_eq!(prompt.options.len(), 2);
        assert_eq!(prompt.options[0].label, "red");
        // Both question ids stashed; the surfaced one recorded.
        let ti = prompt.tool_input.as_ref().unwrap();
        assert_eq!(ti["questionIds"], serde_json::json!(["q1", "q2"]));
        assert_eq!(ti["answeredId"], "q1");
    }

    /// A secret first question is skipped for the first non-secret one; an
    /// all-secret call yields no card (never store secrets).
    #[test]
    fn user_input_card_skips_secret_questions() {
        let id = serde_json::json!(0);
        let mixed = serde_json::json!({
            "questions":[
                {"id":"s1","header":"Token","question":"API token?","isSecret":true,"options":null},
                {"id":"q2","header":"Env","question":"Which env?","isSecret":false,"options":null},
            ],
        });
        let (_, part) = user_input_card(Some("turn1"), &id, &mixed).expect("skips to non-secret");
        let prompt = part.prompt.unwrap();
        assert_eq!(prompt.header.as_deref(), Some("Env"));
        // Still stashes BOTH ids so the reply covers the secret one (empty).
        assert_eq!(
            prompt.tool_input.as_ref().unwrap()["questionIds"],
            serde_json::json!(["s1", "q2"])
        );

        let all_secret = serde_json::json!({
            "questions":[{"id":"s1","question":"secret?","isSecret":true,"options":null}],
        });
        assert!(user_input_card(Some("turn1"), &id, &all_secret).is_none());
    }

    /// The reply fills the surfaced id with the selection/note and every other
    /// stashed id empty; a bare (no selection, no note) answer errs.
    #[test]
    fn user_input_reply_fills_selected_and_empties_the_rest() {
        let prompt = WirePrompt {
            kind: "question".into(),
            tool_input: Some(serde_json::json!({
                "questionIds": ["q1", "q2"],
                "answeredId": "q1",
            })),
            ..Default::default()
        };
        // Selection labels fill q1; q2 gets an empty answer.
        let reply = user_input_reply(&prompt, &answer(true, None, &["red"], None)).unwrap();
        assert_eq!(
            reply["answers"]["q1"]["answers"],
            serde_json::json!(["red"])
        );
        assert_eq!(reply["answers"]["q2"]["answers"], serde_json::json!([]));

        // Note-only (freeform) answers the surfaced id.
        let reply = user_input_reply(&prompt, &answer(true, None, &[], Some("teal"))).unwrap();
        assert_eq!(
            reply["answers"]["q1"]["answers"],
            serde_json::json!(["teal"])
        );

        // Neither selection nor note → Err (card stays actionable).
        assert!(user_input_reply(&prompt, &answer(true, None, &[], None)).is_err());
    }

    /// Numeric question ids stringify to their JSON text as map keys.
    #[test]
    fn user_input_reply_stringifies_numeric_ids() {
        let prompt = WirePrompt {
            kind: "question".into(),
            tool_input: Some(serde_json::json!({
                "questionIds": [1, 2],
                "answeredId": 1,
            })),
            ..Default::default()
        };
        let reply = user_input_reply(&prompt, &answer(true, None, &["x"], None)).unwrap();
        assert_eq!(reply["answers"]["1"]["answers"], serde_json::json!(["x"]));
        assert_eq!(reply["answers"]["2"]["answers"], serde_json::json!([]));
    }

    fn plan_prompt_card() -> WirePrompt {
        // An end-turn plan card: no native_id, so `resume_from_prompt`'s plan
        // arm never touches the host (no busy-check / no client).
        WirePrompt {
            kind: "plan".into(),
            plan: Some("the plan".into()),
            synthesized: true,
            ..Default::default()
        }
    }

    fn test_resume_ctx() -> ResumeCtx {
        ResumeCtx {
            host: std::sync::Arc::new(crate::local::chat::ChatHost::new(
                std::sync::Arc::new(crate::local::opencode::AgentHost::new(None)),
                std::sync::Arc::new(crate::local::codex::CodexHost::new()),
            )),
            session_id: "s".into(),
            native_session_id: None,
        }
    }

    /// The codex plan card resume arms: approve → "Implement the plan." under
    /// Auto (override honored); revise → shared plan-deny wording in Plan mode
    /// (matching Claude); note-less reject → Nothing.
    #[tokio::test]
    async fn plan_resume_arms() {
        let ctx = test_resume_ctx();
        let card = plan_prompt_card();

        // Approve, no note → codex's own phrasing, default Auto.
        let action = Codex
            .resume_from_prompt(&ctx, &card, &answer(true, None, &[], None))
            .await
            .unwrap();
        match action {
            ResumeAction::SendMessage { text, mode } => {
                assert_eq!(text, "Implement the plan.");
                assert_eq!(mode, Some(PermissionMode::Auto));
            }
            _ => panic!("approve should send a message"),
        }

        // Approve with a note + a resume_mode override.
        let action = Codex
            .resume_from_prompt(
                &ctx,
                &card,
                &answer(true, Some("bypass"), &[], Some("skip tests")),
            )
            .await
            .unwrap();
        match action {
            ResumeAction::SendMessage { text, mode } => {
                assert!(text.contains("Implement the plan."));
                assert!(text.contains("skip tests"));
                assert_eq!(mode, Some(PermissionMode::Bypass));
            }
            _ => panic!("approve should send a message"),
        }

        // Revise (note-carrying reject) → shared wording, stays in Plan.
        let action = Codex
            .resume_from_prompt(&ctx, &card, &answer(false, None, &[], Some("tweak X")))
            .await
            .unwrap();
        let (shared_text, shared_mode) =
            synthesize_resume("plan", &answer(false, None, &[], Some("tweak X")));
        match action {
            ResumeAction::SendMessage { text, mode } => {
                assert_eq!(text, shared_text, "revise reuses Claude's wording");
                assert_eq!(mode, shared_mode);
                assert_eq!(mode, Some(PermissionMode::Plan));
            }
            _ => panic!("revise should send a message"),
        }

        // Note-less reject → close the card, no resume.
        let action = Codex
            .resume_from_prompt(&ctx, &card, &answer(false, None, &[], None))
            .await
            .unwrap();
        assert!(matches!(action, ResumeAction::Nothing));
    }

    /// server_req_kind classifies the three reply schemas the settle paths key
    /// on.
    #[test]
    fn server_req_kind_classifies_reply_schemas() {
        use crate::local::codex::{server_req_kind, ServerReqKind};
        assert_eq!(
            server_req_kind("item/commandExecution/requestApproval"),
            ServerReqKind::Approval
        );
        assert_eq!(
            server_req_kind("item/fileChange/requestApproval"),
            ServerReqKind::Approval
        );
        assert_eq!(
            server_req_kind("item/tool/requestUserInput"),
            ServerReqKind::UserInput
        );
        // A reply schema we don't speak (permission-profile object).
        assert_eq!(
            server_req_kind("item/permissions/requestApproval"),
            ServerReqKind::Other
        );
    }
}
