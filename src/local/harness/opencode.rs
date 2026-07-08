//! OpenCode harness.
//!
//! Chat: talks to a lazily spawned `opencode serve` child (the `AgentHost` the
//! up server shares). serve is opencode's first-party embedding surface; HTTP
//! on loopback is just this adapter's transport, never exposed to the browser.
//! A turn = subscribe to the global `/event` SSE stream, POST the message
//! (which resolves when the turn ends), and translate this session's part
//! events into wire parts as they stream.
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
use super::Harness;
use crate::error::{anyhow, Result};
use crate::local::chat::{TurnCtx, WirePart, WireToolState};
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
            })
        }
        _ => None,
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

    let mut body = json!({ "parts": [{ "type": "text", "text": ctx.text }] });
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
                    handle_event(ctx, &native_id, &event, &mut assistant_msgs);
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
