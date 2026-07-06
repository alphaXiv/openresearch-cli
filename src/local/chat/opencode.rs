//! OpenCode adapter: talks to a lazily spawned `opencode serve` child (the
//! `AgentHost` the up server shares). serve is opencode's first-party
//! embedding surface — HTTP on loopback is just this adapter's transport,
//! never exposed to the browser.
//!
//! A turn = subscribe to the global `/event` SSE stream, POST the message
//! (which resolves when the turn ends), and translate this session's part
//! events into wire parts as they stream.

use std::collections::HashSet;

use futures::StreamExt;
use serde_json::{json, Value};

use crate::error::{anyhow, Result};
use crate::local::chat::{TurnCtx, WirePart, WireToolState};

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

pub async fn run_turn(ctx: &mut TurnCtx) -> Result<()> {
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
