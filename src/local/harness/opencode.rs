//! OpenCode harness.
//!
//! Chat: talks to a lazily spawned `opencode serve` child (the `AgentHost` the
//! up server shares). serve is opencode's first-party embedding surface; HTTP
//! on loopback is just this adapter's transport, never exposed to the browser.
//! A turn = subscribe to the global `/event` SSE stream, POST the message
//! (which resolves when the turn ends), and translate this session's part
//! events into wire parts as they stream.
//!
//! Interactive prompts: unlike Claude (which ends its turn and resumes with a
//! new message), opencode approves *inline*. Its serve stream emits
//! `permission.asked` / `question.asked` while the `session.prompt` POST is
//! still open — the turn is paused, not finished. We surface those as
//! `permission` / `question` cards and reply over the live session
//! (`resume_from_prompt` → [`ResumeAction::Handled`]), which unblocks the same
//! POST. `Bypass` mode auto-resolves permission cards (replies "always" without
//! a blocking card); `Auto`/`Plan` surface them. Questions always need a human,
//! so they always surface regardless of mode.
//!
//! Detection: opencode's `auth.json` is `{provider: {type}}`; the signed-in
//! providers are its account line, and `opencode models` is the model list.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};

use super::detect::{bin_version, read_json, HarnessInfo};
use super::options::{HarnessOptions, PermissionMode};
use super::{Harness, ResumeAction};
use crate::error::{anyhow, Result};
use crate::local::chat::{
    PromptAnswer, ResumeCtx, TurnCtx, WirePart, WirePrompt, WireQuestionOption, WireToolState,
};
use crate::local::opencode::find_opencode;

pub struct OpenCode;

#[async_trait]
impl Harness for OpenCode {
    fn id(&self) -> &'static str {
        "opencode"
    }

    fn name(&self) -> &'static str {
        "OpenCode"
    }

    fn supports_chat(&self) -> bool {
        true
    }

    async fn detect(&self) -> Option<HarnessInfo> {
        let mut info = HarnessInfo::new(self.id(), self.name());
        let mut models = Vec::new();
        if let Ok(bin) = find_opencode() {
            info.installed = true;
            info.version = bin_version(&bin).await;
            models = opencode_models(&bin).await;
            info.bin_path = Some(bin.to_string_lossy().into_owned());
        }
        let providers = opencode_providers();
        if !providers.is_empty() {
            info.authenticated = true;
            info.auth_method = Some("oauth");
            info.account = Some(providers.join(", "));
        }

        info.agent_ready = info.installed;
        if info.agent_ready {
            info.models = models
                .into_iter()
                .map(|id| super::ModelInfo { id })
                .collect();
        } else {
            info.agent_note = Some(
                "Install opencode (curl -fsSL https://opencode.ai/install | bash) to chat with it here."
                    .to_string(),
            );
        }
        Some(info)
    }

    async fn run_turn(&self, ctx: &mut TurnCtx) -> Result<()> {
        run_turn(ctx).await
    }

    fn options(&self) -> HarnessOptions {
        // Two native OpenCode axes folded onto the one Mode toggle:
        //  * which built-in agent runs — `plan` (read-only: allows inspection
        //    like `orx …`, denies edits) vs `build` (the default). A real, clean
        //    plan mode, unlike Claude/Codex — verified live.
        //  * how a `permission.asked` is answered. NOTE opencode's default is
        //    permissive (`allow *`); it only prompts on a few risky cases
        //    (runaway loops, out-of-workspace writes, `.env` reads), so a
        //    dedicated "ask for everything" mode would be hollow (cards would
        //    almost never fire). So we don't offer one:
        //      * Plan   → plan agent, and surface the rare cards that do fire.
        //      * Auto   → build agent, opencode's permissive default (still
        //                 surfaces those rare cards / questions).
        //      * Bypass → build agent, auto-approve even those.
        // No reasoning control — reasoning is a model property in opencode.
        HarnessOptions::none().with_permission_modes(
            &[
                PermissionMode::Plan,
                PermissionMode::Auto,
                PermissionMode::Bypass,
            ],
            PermissionMode::Auto,
        )
    }

    /// opencode is paused mid-turn on a `permission.asked` / `question.asked`;
    /// the answer is replied over the live serve session, which unblocks the
    /// still-open `session.prompt` POST. So this delivers the reply inline and
    /// returns [`ResumeAction::Handled`] — never the new-message path.
    async fn resume_from_prompt(
        &self,
        ctx: &ResumeCtx,
        prompt: &WirePrompt,
        answer: &PromptAnswer,
    ) -> Result<ResumeAction> {
        reply_inline(ctx, prompt, answer).await?;
        Ok(ResumeAction::Handled)
    }

    fn config_home(&self) -> Option<PathBuf> {
        // OpenCode discovers skills under XDG config, staying XDG even on macOS.
        Some(super::xdg_config_home().join("opencode"))
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
        // OpenCode reads the same SKILL.md format as Claude Code.
        Some(super::CLAUDE_SKILL)
    }
}

fn opencode_auth_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("share")))?;
    Some(base.join("opencode").join("auth.json"))
}

/// Providers opencode is signed into (its auth.json is `{provider: {type}}`).
fn opencode_providers() -> Vec<String> {
    let Some(auth) = opencode_auth_path().and_then(read_json) else {
        return Vec::new();
    };
    match auth.as_object() {
        Some(map) => map.keys().cloned().collect(),
        None => Vec::new(),
    }
}

/// `opencode models` — the ground truth for what the agent can actually run.
async fn opencode_models(bin: &PathBuf) -> Vec<String> {
    let fut = tokio::process::Command::new(bin)
        .arg("models")
        .current_dir(dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")))
        .stdin(std::process::Stdio::null())
        .output();
    let Ok(Ok(out)) = tokio::time::timeout(Duration::from_secs(20), fut).await else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && l.contains('/'))
        .map(str::to_string)
        .collect()
}

/// opencode part → wire part (the shapes are already close).
fn to_wire_part(part: &Value) -> Option<WirePart> {
    let id = part.get("id")?.as_str()?.to_string();
    let kind = part.get("type")?.as_str()?;
    match kind {
        "text" | "reasoning" => Some(WirePart {
            id,
            kind: kind.into(),
            text: part.get("text").and_then(Value::as_str).map(str::to_string),
            tool: None,
            state: None,
            prompt: None,
        }),
        "tool" => {
            let state = part.get("state");
            Some(WirePart {
                id,
                kind: "tool".into(),
                text: None,
                tool: part.get("tool").and_then(Value::as_str).map(str::to_string),
                state: Some(WireToolState {
                    status: state
                        .and_then(|s| s.get("status"))
                        .and_then(Value::as_str)
                        .unwrap_or("running")
                        .into(),
                    input: state.and_then(|s| s.get("input")).cloned(),
                    output: state
                        .and_then(|s| s.get("output"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    error: state
                        .and_then(|s| s.get("error"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    title: state
                        .and_then(|s| s.get("title"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                }),
                prompt: None,
            })
        }
        _ => None,
    }
}

/// opencode `permission.asked` payload → a `permission` card. The permission
/// request id rides on `native_id` so the reply can address
/// `POST /session/{sid}/permissions/{id}`. `permission` is opencode's tool
/// group (e.g. `bash`, `edit`); the metadata carries the concrete call detail.
fn permission_card(props: &Value) -> Option<WirePrompt> {
    let id = props.get("id").and_then(Value::as_str)?.to_string();
    Some(WirePrompt {
        kind: "permission".into(),
        tool: props
            .get("permission")
            .and_then(Value::as_str)
            .map(str::to_string),
        // The event's `metadata` is the closest thing to a tool input summary
        // the UI can render (command / file / etc., shape varies by tool).
        tool_input: props.get("metadata").filter(|m| !m.is_null()).cloned(),
        native_id: Some(id),
        ..Default::default()
    })
}

/// opencode `question.asked` payload → a `question` card. opencode's
/// `QuestionInfo` (`{question, header, options:[{label,description}], multiple}`)
/// is the same shape as Claude's AskUserQuestion, so it maps 1:1. Only the first
/// question is surfaced (the composer answers one at a time); its request id
/// rides on `native_id` for `POST /question/{id}/reply`.
fn question_card(props: &Value) -> Option<WirePrompt> {
    let id = props.get("id").and_then(Value::as_str)?.to_string();
    let q = props
        .get("questions")
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
        multi_select: q.get("multiple").and_then(Value::as_bool).unwrap_or(false),
        native_id: Some(id),
        ..Default::default()
    })
}

/// POST a permission decision to the live serve session (v1 API). `response` is
/// `once` | `always` | `reject`.
async fn post_permission(
    http: &reqwest::Client,
    base: &str,
    native_session: &str,
    permission_id: &str,
    response: &str,
) -> Result<()> {
    http.post(format!(
        "{base}/session/{native_session}/permissions/{permission_id}"
    ))
    .json(&json!({ "response": response }))
    .send()
    .await?
    .error_for_status()?;
    Ok(())
}

/// Deliver an answered card's reply to the live serve session, unblocking the
/// paused `session.prompt` POST. Permission → `{response: once|always|reject}`;
/// question → `{answers: [[label,...]]}` (or reject). The reply target is the
/// card's `native_id` (the opencode permission/question request id).
async fn reply_inline(ctx: &ResumeCtx, prompt: &WirePrompt, answer: &PromptAnswer) -> Result<()> {
    let request_id = prompt
        .native_id
        .as_deref()
        .ok_or_else(|| anyhow!("opencode prompt has no reply id"))?;
    // The reply only lands if the turn is still paused waiting for it. If the
    // turn already ended (errored / interrupted), serve may still accept the
    // POST but no one is consuming the resumed stream, so the reply would be
    // lost and the card would falsely mark resolved. Reject it instead — the
    // card stays actionable and the user sees the turn is no longer live.
    if !ctx.is_busy().await {
        return Err(anyhow!(
            "this turn is no longer running — its prompt can't be answered"
        ));
    }
    // Reach the live serve child through the shared host, exactly as
    // `ChatHost::interrupt` does — the reply goes to the same loopback serve
    // whose `session.prompt` POST is paused on this prompt.
    let port = ctx
        .host
        .opencode
        .proxy_port()
        .await
        .ok_or_else(|| anyhow!("opencode serve is not running — cannot deliver the reply"))?;
    let base = format!("http://127.0.0.1:{port}");
    let http = ctx.http();

    match prompt.kind.as_str() {
        "permission" => {
            // approve → "always" (so the same tool won't re-prompt this turn);
            // reject closes it. The reply is session-scoped in opencode's v1 API.
            let native_session = ctx.native_session_id.as_deref().ok_or_else(|| {
                anyhow!("opencode session has no native id — cannot deliver the reply")
            })?;
            let response = if answer.approve { "always" } else { "reject" };
            post_permission(http, &base, native_session, request_id, response).await?;
        }
        "question" => {
            if answer.answers.is_empty() {
                // No selection: reject the question rather than reply empty, so
                // opencode surfaces the model's fallback path.
                http.post(format!("{base}/question/{request_id}/reject"))
                    .json(&json!({}))
                    .send()
                    .await?
                    .error_for_status()?;
            } else {
                // opencode takes an array of answers, one per question; we only
                // surface the first question, so send a single answer array.
                http.post(format!("{base}/question/{request_id}/reply"))
                    .json(&json!({ "answers": [&answer.answers] }))
                    .send()
                    .await?
                    .error_for_status()?;
            }
        }
        other => {
            return Err(anyhow!(
                "opencode cannot reply to a `{other}` prompt inline"
            ))
        }
    }
    Ok(())
}

/// Session mode → opencode built-in agent name. `Plan` runs the read-only
/// `plan` agent (denies edits, allows inspection); everything else runs the
/// default `build` agent. The permission-reply behavior (surface vs auto-reply)
/// is a separate axis handled in `handle_prompt_event`.
fn opencode_agent(mode: Option<PermissionMode>) -> &'static str {
    match mode {
        Some(PermissionMode::Plan) => "plan",
        _ => "build",
    }
}

async fn run_turn(ctx: &mut TurnCtx) -> Result<()> {
    // Lazy bring-up: spawns serve for this project or reuses the live child.
    let status = ctx.host.opencode.ensure(&ctx.project).await?;
    let port = status
        .port
        .ok_or_else(|| anyhow!("opencode agent has no port"))?;
    let base = format!("http://127.0.0.1:{port}");

    let native_id = match &ctx.native_session_id {
        Some(id) => id.clone(),
        None => {
            let session: Value = ctx
                .http()
                .post(format!("{base}/session"))
                .header("content-type", "application/json")
                .body("{}")
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let id = session
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("opencode session response had no id"))?
                .to_string();
            ctx.set_native_session_id(&id);
            id
        }
    };

    // Subscribe before sending so no early part events are missed.
    let events = ctx
        .http()
        .get(format!("{base}/event"))
        .send()
        .await?
        .error_for_status()?;
    let mut stream = events.bytes_stream();

    let mut body = json!({
        "parts": [{ "type": "text", "text": ctx.text }],
        // Select opencode's built-in agent from the session's mode: `plan` (the
        // read-only planning agent — allows inspection, denies edits) vs `build`
        // (the default). The message endpoint takes `agent` directly (verified),
        // so no separate switch call is needed.
        "agent": opencode_agent(ctx.permission_mode),
    });
    if let Some(model) = &ctx.model {
        if let Some((provider, model_id)) = model.split_once('/') {
            body["model"] = json!({ "providerID": provider, "modelID": model_id });
        }
    }
    let send = ctx
        .http()
        .post(format!("{base}/session/{native_id}/message"))
        .json(&body)
        .send();
    tokio::pin!(send);

    // Parts are attributed via message.updated role info; a part arriving
    // before its message would be misfiled, and assistant messages are always
    // announced before their parts stream.
    let mut assistant_msgs: HashSet<String> = HashSet::new();
    let mut buf = String::new();

    loop {
        tokio::select! {
            chunk = stream.next() => {
                let Some(chunk) = chunk else {
                    return Err(anyhow!("opencode event stream ended mid-turn"));
                };
                buf.push_str(&String::from_utf8_lossy(&chunk?));
                while let Some(pos) = buf.find('\n') {
                    let line = buf[..pos].trim().to_string();
                    buf.drain(..=pos);
                    let Some(data) = line.strip_prefix("data: ") else { continue };
                    let Ok(event) = serde_json::from_str::<Value>(data) else { continue };
                    // Interactive prompts (permission/question) pause the turn and
                    // are handled async (emit a card, or auto-reply per mode); all
                    // other events are message/part updates handled synchronously.
                    if !handle_prompt_event(ctx, &native_id, &base, &event).await? {
                        handle_event(ctx, &native_id, &event, &mut assistant_msgs);
                    }
                }
            }
            resp = &mut send => {
                // Turn done — the response body is the final assistant message;
                // merge its parts as the authoritative versions.
                let resp = resp?.error_for_status()?;
                if let Ok(message) = resp.json::<Value>().await {
                    if let Some(parts) = message.get("parts").and_then(Value::as_array) {
                        for part in parts {
                            if let Some(wire) = to_wire_part(part) {
                                ctx.upsert_part(wire);
                            }
                        }
                    }
                }
                return Ok(());
            }
        }
    }
}

fn handle_event(
    ctx: &mut TurnCtx,
    native_id: &str,
    event: &Value,
    assistant_msgs: &mut HashSet<String>,
) {
    let props = event.get("properties").unwrap_or(&Value::Null);
    match event.get("type").and_then(Value::as_str) {
        Some("message.updated") => {
            let info = props.get("info").unwrap_or(&Value::Null);
            if info.get("sessionID").and_then(Value::as_str) == Some(native_id)
                && info.get("role").and_then(Value::as_str) == Some("assistant")
            {
                if let Some(id) = info.get("id").and_then(Value::as_str) {
                    assistant_msgs.insert(id.to_string());
                }
            }
        }
        Some("message.part.updated") => {
            let part = props.get("part").unwrap_or(&Value::Null);
            if part.get("sessionID").and_then(Value::as_str) != Some(native_id) {
                return;
            }
            let owned_by_assistant = part
                .get("messageID")
                .and_then(Value::as_str)
                .is_some_and(|mid| assistant_msgs.contains(mid));
            if !owned_by_assistant {
                return;
            }
            if let Some(wire) = to_wire_part(part) {
                ctx.upsert_part(wire);
                ctx.maybe_flush();
            }
        }
        Some("message.part.delta") => {
            if props.get("sessionID").and_then(Value::as_str) != Some(native_id) {
                return;
            }
            if props.get("field").and_then(Value::as_str) != Some("text") {
                return;
            }
            if let (Some(part_id), Some(delta)) = (
                props.get("partID").and_then(Value::as_str),
                props.get("delta").and_then(Value::as_str),
            ) {
                ctx.append_part_text(part_id, delta);
                ctx.maybe_flush();
            }
        }
        Some("session.updated") => {
            // Adopt opencode's auto-generated titles.
            let info = props.get("info").unwrap_or(&Value::Null);
            if info.get("id").and_then(Value::as_str) == Some(native_id) {
                if let Some(title) = info.get("title").and_then(Value::as_str) {
                    ctx.set_title(title);
                }
            }
        }
        _ => {}
    }
}

/// Surface a prompt card and flush it so it renders immediately (before the
/// turn resumes). The card's `native_id` (the reply target) is also its
/// `WirePart` id, so the user's answer round-trips back to the right request.
fn surface_card(ctx: &mut TurnCtx, card: WirePrompt) {
    // `native_id` is always set by permission_card/question_card (opencode
    // requires the request id); the fallback id only guards a malformed payload.
    let part_id = card
        .native_id
        .clone()
        .unwrap_or_else(|| format!("prompt-{}", ctx.assistant.parts.len()));
    ctx.upsert_part(WirePart::prompt(part_id, card));
    let _ = ctx.flush();
}

/// Handle an interactive-prompt SSE event (`permission.asked` / `question.asked`)
/// for this session. Returns `true` if it consumed the event (so the caller
/// skips `handle_event`), `false` otherwise.
///
/// Permissions honor the session's mode: `Auto`/`Bypass` auto-reply `always`
/// over the live session (no blocking card); anything else surfaces a card and
/// pauses. Questions always surface — there's no sensible auto-answer. A single
/// flaky auto-reply must not lose the whole turn, so on POST failure we fall
/// back to surfacing the card rather than propagating the error.
async fn handle_prompt_event(
    ctx: &mut TurnCtx,
    native_id: &str,
    base: &str,
    event: &Value,
) -> Result<bool> {
    let props = event.get("properties").unwrap_or(&Value::Null);
    // Only this session's prompts (the /event stream is global across sessions).
    if props.get("sessionID").and_then(Value::as_str) != Some(native_id) {
        // Not a match — but if it *is* a prompt event for another session, still
        // report "not consumed" so handle_event ignores it too (it will, by id).
        return Ok(false);
    }
    match event.get("type").and_then(Value::as_str) {
        Some("permission.asked") => {
            let Some(card) = permission_card(props) else {
                // No request id to reply to — surface it as an error so the turn
                // isn't silently wedged waiting on an answer no one can give.
                ctx.push_error("opencode asked for a permission we couldn't parse".into());
                let _ = ctx.flush();
                return Ok(true);
            };
            // Only Bypass auto-approves. Auto is opencode's permissive default —
            // the rare card it does raise (out-of-workspace write, `.env` read)
            // is worth surfacing; Plan surfaces them too.
            let auto_approve = matches!(ctx.permission_mode, Some(PermissionMode::Bypass));
            match (auto_approve, card.native_id.as_deref()) {
                (true, Some(id)) => {
                    // Reply without surfacing a card — keep the turn flowing. If
                    // the reply POST fails, don't kill the turn: fall back to a
                    // card so the user can decide.
                    if let Err(err) =
                        post_permission(ctx.http(), base, native_id, id, "always").await
                    {
                        eprintln!("orx up: opencode auto-approve failed, surfacing card: {err}");
                        surface_card(ctx, card);
                    }
                }
                _ => surface_card(ctx, card),
            }
            Ok(true)
        }
        Some("question.asked") => {
            match question_card(props) {
                Some(card) => surface_card(ctx, card),
                None => {
                    ctx.push_error("opencode asked a question we couldn't parse".into());
                    let _ = ctx.flush();
                }
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_mode_uses_the_plan_agent_others_build() {
        assert_eq!(opencode_agent(Some(PermissionMode::Plan)), "plan");
        assert_eq!(opencode_agent(Some(PermissionMode::Ask)), "build");
        assert_eq!(opencode_agent(Some(PermissionMode::Auto)), "build");
        assert_eq!(opencode_agent(Some(PermissionMode::Bypass)), "build");
        // No mode set → the default build agent, never plan.
        assert_eq!(opencode_agent(None), "build");
    }
}
