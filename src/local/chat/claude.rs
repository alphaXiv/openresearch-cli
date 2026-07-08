//! Claude Code adapter: one `claude --print` process per turn, stream-json on
//! stdout, multi-turn via `--resume` against Claude Code's own session store.
//! The playbook rides `--append-system-prompt-file`; permissions are bypassed
//! (parity with the opencode allow-all config — the playbook forbids
//! interactive questions anyway, and headless mode couldn't answer them).

use std::path::PathBuf;
use std::process::Stdio;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::error::{anyhow, Result};
use crate::local::chat::{prepare_env, TurnCtx, WirePart, WireToolState};
use crate::local::opencode::ensure_playbook;

/// `claude` on PATH, else the common install drop locations.
pub fn find_claude() -> Option<PathBuf> {
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join("claude");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let home = dirs::home_dir()?;
    [".claude/local/claude", ".local/bin/claude"]
        .iter()
        .map(|rel| home.join(rel))
        .find(|c| c.is_file())
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

pub async fn run_turn(ctx: &mut TurnCtx) -> Result<()> {
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
        "bypassPermissions",
        // Headless --print can't answer it (auto-dismissed); parity with the
        // opencode config's `question: false`.
        "--disallowed-tools",
        "AskUserQuestion",
    ])
    .arg("--append-system-prompt-file")
    .arg(&playbook)
    .current_dir(&repo)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::from(super::harness_log("claude")?))
    .kill_on_drop(true);
    if let Some(model) = &ctx.model {
        cmd.args(["--model", model]);
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
                            ctx.upsert_part(WirePart {
                                id,
                                kind: "tool".into(),
                                text: None,
                                tool: block
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .map(str::to_string),
                                state: Some(WireToolState {
                                    status: "running".into(),
                                    input: block.get("input").map(normalize_input),
                                    output: None,
                                    error: None,
                                    title: None,
                                }),
                            });
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
                let subtype = event.get("subtype").and_then(Value::as_str).unwrap_or("");
                if subtype != "success" {
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
