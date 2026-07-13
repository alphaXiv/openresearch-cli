//! Codex harness.
//!
//! Chat rides Codex's **app-server** protocol (codex ≥ 0.144): one long-lived
//! `codex app-server` child per session (see `local::codex`), a thread per
//! session (`thread/start` / `thread/resume` — the thread id persists as the
//! session's `native_session_id`), one `turn/start` per message, events
//! streamed as JSON-RPC notifications. The playbook rides
//! `developerInstructions` (a real instruction channel — no more first-turn
//! `<system-context>` text wrapping), and the sandbox policy travels per turn
//! (`sandboxPolicy` with writable roots + network). Verified against
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

use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use super::detect::{
    bin_version, find_on_path, jwt_payload, nonempty_str, read_json, resolve_symlinks, title_case,
    HarnessInfo,
};
use super::options::{HarnessOptions, PermissionMode};
use super::Harness;
use crate::error::{anyhow, Result};
use crate::local::chat::{prepare_env, TurnCtx, WirePart, WireToolState};
use crate::local::codex::{CodexClient, TurnEvent};
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
            // app-server wins (thread resume; the approval channel the
            // follow-up builds on).
            let too_old = info
                .version
                .as_deref()
                .and_then(parse_codex_version)
                .is_some_and(|v| v < MIN_APP_SERVER_VERSION);
            if too_old {
                info.agent_note = Some(
                    "This Codex version chats via the legacy exec path — update to 0.144+ for the app-server integration.".to_string(),
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
        // Approvals are `never` on both transports this release, so permission
        // modes map onto the *sandbox policy*; we offer only Auto + Bypass,
        // matching Claude. On app-server that's a deliberate hold — the pure
        // transport swap (see `codex_policies`); the follow-up flips Auto to
        // `on-request` and can then revisit Codex's first-class Plan mode,
        // which app-server does support. On the legacy exec fallback there is
        // no approval channel at all, and a `Plan`→`read-only` sandbox was
        // dropped for the same reason as Claude's: read-only blocks the `orx`
        // inspection the agent needs *and* the launches that are the point.
        //   * Auto  — workspace-write, plus network, the orx data dir, and
        //     the hub clone's `.git` (see `sandbox_policy_json`, and
        //     `run_turn_exec` for the legacy `-c` form).
        //   * Bypass— full access.
        HarnessOptions::none()
            .with_permission_modes(
                &[PermissionMode::Auto, PermissionMode::Bypass],
                PermissionMode::Auto,
            )
            // Codex's own reasoning tiers via `-c model_reasoning_effort`.
            .with_reasoning_levels(&CODEX_REASONING_LEVELS, "high")
    }

    fn config_home(&self) -> Option<PathBuf> {
        Some(dirs::home_dir()?.join(".codex"))
    }

    fn skill_target(&self) -> Option<PathBuf> {
        Some(self.config_home()?.join("prompts").join("orx.md"))
    }

    fn skill_shim(&self) -> Option<&'static str> {
        Some(super::CODEX_PROMPT)
    }
}

// --- app-server path (codex ≥ 0.144) -----------------------------------------

/// First protocol version the harness was validated against (schema dump +
/// live spike). Older CLIs take the exec fallback below.
const MIN_APP_SERVER_VERSION: (u64, u64, u64) = (0, 144, 0);

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

/// Session mode → (thread `sandbox` mode, `approvalPolicy`). Approvals stay
/// `never` on every mode for now — the sandbox is still the boundary, exactly
/// like the exec path — so this PR is a pure transport swap. The follow-up
/// flips Auto to `on-request`, surfacing sandbox escalations as permission
/// cards (verified live: 0.144 asks *before* running an out-of-sandbox
/// command).
fn codex_policies(mode: Option<PermissionMode>) -> (&'static str, &'static str) {
    match mode.unwrap_or(PermissionMode::Auto) {
        PermissionMode::Bypass => ("danger-full-access", "never"),
        // Plan/AcceptEdits/Ask have no distinct semantics here (mirrors
        // `codex_sandbox` on the exec path): the balanced default.
        _ => ("workspace-write", "never"),
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

/// How a turn ended, from `turn/completed`.
enum TurnEnd {
    Done,
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
            return Some(TurnEnd::Done); // completed | interrupted
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
        // userMessage (our own echo), mcpToolCall, webSearch, plan, …: not
        // rendered (parity with the exec path); unknown types tolerated.
        _ => {}
    }
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
    let project = ctx.project.clone();
    let session_id = ctx.session_id.clone();
    let (repo, playbook) =
        tokio::task::spawn_blocking(move || ensure_playbook(&project, &session_id))
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
                Ok(_) => {
                    client.set_resumed_thread(&id);
                    id
                }
                // Codex *rejected* the id (e.g. minted by the old exec path,
                // or the rollout is gone): start a fresh thread; prior context
                // is lost either way. A transport failure, by contrast,
                // propagates as the turn's error (the `?` above) — a resumable
                // thread must never be discarded over a timeout/hiccup.
                Err(err) => {
                    eprintln!("orx up: codex thread/resume rejected ({err}); starting a fresh thread");
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
    if let Some(effort) = codex_reasoning(ctx.reasoning_level.as_deref()) {
        turn_params["effort"] = Value::String(effort.to_string());
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

    while let Some(event) = rx.recv().await {
        match event {
            TurnEvent::Notification { method, params } => {
                if event_turn_mismatch(turn_id.as_deref(), &params) {
                    continue;
                }
                match apply_notification(ctx, &method, &params) {
                    Some(TurnEnd::Done) => {
                        let _ = ctx.flush();
                        return Ok(());
                    }
                    Some(TurnEnd::Failed(message)) => {
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
            TurnEvent::Request { id, .. } => {
                // approvalPolicy is `never` on every mode, so no approval
                // should arrive; decline defensively rather than leave the
                // server blocked on an unanswered request. The follow-up
                // surfaces these as permission cards — and must keep declining
                // stale-turn requests (params carry turnId; `event_turn_mismatch`
                // works on them unchanged) instead of surfacing them.
                let _ = client.respond_decline(&id).await;
            }
            TurnEvent::Closed => {
                return Err(anyhow!(
                    "codex app-server exited mid-turn; see {}",
                    crate::store::data_dir().join("agent-codex.log").display()
                ));
            }
        }
        ctx.maybe_flush();
    }
    Err(anyhow!("codex app-server event stream ended mid-turn"))
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
    let (repo, playbook) =
        tokio::task::spawn_blocking(move || ensure_playbook(&project, &session_id))
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
        // Every non-bypass mode is the balanced sandbox; approvals stay off
        // in this release (pure transport swap — see codex_policies docs).
        assert_eq!(codex_policies(None), ("workspace-write", "never"));
        assert_eq!(
            codex_policies(Some(PermissionMode::Auto)),
            ("workspace-write", "never")
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
        assert!(matches!(ended, Some(TurnEnd::Done)));

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
        // Interrupted is a clean end, not a failure.
        let end = apply_notification(
            &mut ctx,
            "turn/completed",
            &serde_json::json!({"turn":{"id":"t","status":"interrupted"}}),
        );
        assert!(matches!(end, Some(TurnEnd::Done)));
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
}
