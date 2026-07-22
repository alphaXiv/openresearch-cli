//! Persistent Claude Code host — one long-lived `claude --print --input-format
//! stream-json` child per chat session, mirroring `AgentHost`/`CodexHost`. The
//! child stays resident across turns: a stable `session_id` survives every turn,
//! stdin never closes, and each user message is one JSON line
//! (`{"type":"user","message":{"role":"user","content":[…]}}`). This replaces
//! today's spawn-per-turn + `--resume` fork (`harness/claude.rs`'s old
//! `run_turn`), whose ~3–6s of per-turn spawn/config/MCP-handshake overhead the
//! latency probe measured.
//!
//! Wire shapes were pinned live against claude CLI 2.1.197. In persistent mode
//! `--print --input-format stream-json --output-format stream-json` accepts one
//! user message per stdin line and ends each turn with a `result` event; the
//! `session_id` is stable across turns in one process, and `--resume <id>`
//! composes with stream-json input, so a crash/restart or config-change respawn
//! recovers cheaply. Control requests ride the same stdin stream
//! (`{"type":"control_request","request_id":…,"request":{"subtype":…}}`) and are
//! answered by a `control_response`; `set_model` is the only one we use — native
//! `interrupt` gave no response and is treated as unreliable, so v1 interrupts
//! by killing the child (`ChatHost::interrupt`'s `claude-code` branch).
//!
//! Non-goals (separate tasks): MCP config isolation (`--strict-mcp-config`);
//! codex/opencode paths; the UI rendering fix.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::error::{anyhow, Result};
use crate::local::harness::claude::{
    claude_permission_mode, find_claude, write_mcp_config, write_plan_settings,
};
use crate::local::harness::PermissionMode;

/// Ceiling on a control request's response wait. Control requests (`set_model`)
/// are quick; a wedged child must never hold the respawn path for long.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);

/// What the resident child was spawned with. Any field that can only be applied
/// at launch (permission mode, effort, the mcp-gate bridge) forces a respawn
/// when it changes; `model` is the exception — it retunes live via `set_model`,
/// so it is updated in place on a successful control request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnConfig {
    pub permission_mode: Option<PermissionMode>,
    pub effort: Option<String>,
    pub model: Option<String>,
    /// Whether the mcp-gate permission bridge was actually wired at spawn (plan
    /// mode + a bound `orx up` port + a successful config write). A plan turn
    /// that wanted the bridge but got `false` respawns next turn to try again.
    pub bridge_active: bool,
}

/// Everything needed to spawn (or respawn) a session's child, built per turn by
/// the harness. `resume` carries the native session id for `--resume` recovery
/// (crash/restart or config-change respawn); `None` on a session's first spawn.
/// `chat` is the spawn-time handle for the plan-mode bridge (`up_port` +
/// `mint_gate_token`) — the spec is transient (built per turn, consumed by
/// `ensure`), so the strong ref cannot cycle.
pub struct SpawnSpec {
    pub chat: Arc<crate::local::chat::ChatHost>,
    pub session_id: String,
    pub repo: PathBuf,
    pub playbook: PathBuf,
    pub resume: Option<String>,
    pub config: SpawnConfig,
}

/// One event delivered to the session's in-flight turn.
#[derive(Debug)]
pub enum TurnEvent {
    /// A stream-json output line (an `assistant`/`user`/`system`/`result`
    /// object). The turn loop folds these through `apply_event`.
    Line(Value),
    /// Child died (EOF on stdout). The turn cannot continue.
    Closed,
}

/// One inbound stdout line, classified. Claude interleaves two shapes on the
/// same stream: `control_response` (settling one of our control requests) and
/// ordinary stream-json output objects (turn events).
#[derive(Debug, PartialEq)]
pub enum ClaudeLine {
    /// A `control_response` for a request we sent: `request_id` echoed back,
    /// plus `Ok`/`Err` from the response `subtype`.
    ControlResponse {
        request_id: String,
        result: Result<Value, String>,
    },
    /// A stream-json output object (turn event) — routed to the live turn.
    Event(Value),
    /// Unparseable, or a control message we don't originate (`control_request`
    /// from the child). Ignored.
    Junk,
}

/// Classify one stdout line. Pure — the reader task and the tests share it.
pub fn classify_line(line: &str) -> ClaudeLine {
    let Ok(msg) = serde_json::from_str::<Value>(line) else {
        return ClaudeLine::Junk;
    };
    match msg.get("type").and_then(Value::as_str) {
        Some("control_response") => {
            let response = msg.get("response").unwrap_or(&Value::Null);
            let Some(request_id) = response
                .get("request_id")
                .and_then(Value::as_str)
                .map(str::to_string)
            else {
                return ClaudeLine::Junk;
            };
            let result = match response.get("subtype").and_then(Value::as_str) {
                Some("success") => Ok(response.get("response").cloned().unwrap_or(Value::Null)),
                _ => Err(response
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("claude control request failed")
                    .to_string()),
            };
            ClaudeLine::ControlResponse { request_id, result }
        }
        // A control_request FROM the child (e.g. a permission prompt) rides the
        // mcp-gate bridge over HTTP, not this stream — we never answer it here.
        Some("control_request") | None => ClaudeLine::Junk,
        // Everything else is a stream-json output object: assistant/user/system/
        // result. The turn loop discriminates on `type`.
        Some(_) => ClaudeLine::Event(msg),
    }
}

/// A live connection to one session's resident `claude` child.
pub struct ClaudeClient {
    child: Mutex<Child>,
    /// Held open for the life of the child: stdin EOF ends the session, so we
    /// never drop it between turns. One JSON line per user message / control
    /// request.
    stdin: Mutex<ChildStdin>,
    next_id: AtomicI64,
    /// Our outstanding control requests. Sync mutex: touched from the reader
    /// task and from `control_request`, held only for map ops, never across an
    /// await.
    pending: std::sync::Mutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>,
    /// The session's in-flight turn, if any (one per session-child). The
    /// registration guard drops synchronously on task abort, hence sync mutex.
    turn: std::sync::Mutex<Option<mpsc::UnboundedSender<TurnEvent>>>,
    /// What this child was spawned with — the reuse/respawn decision reads it,
    /// and a successful `set_model` updates its `model` in place.
    config: std::sync::Mutex<SpawnConfig>,
}

impl ClaudeClient {
    /// The config this child is currently running under.
    pub fn config(&self) -> SpawnConfig {
        self.config.lock().unwrap().clone()
    }

    /// Send a user message as one stream-json line. The child answers with a
    /// stream of output objects ending in a `result`.
    pub async fn send_user_message(&self, text: &str) -> Result<()> {
        let msg = json!({
            "type": "user",
            "message": { "role": "user", "content": [{ "type": "text", "text": text }] },
        });
        self.write_line(&msg).await
    }

    /// Send a control request and await its `control_response`. `Ok(Err(msg))`
    /// is a *child-reported* failure, `Err(..)` a transport failure (child gone
    /// / timeout).
    async fn control_request(&self, request: Value) -> Result<Result<Value, String>> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request_id = format!("req_{id}");
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(request_id.clone(), tx);
        let sent = self
            .write_line(&json!({
                "type": "control_request",
                "request_id": request_id,
                "request": request,
            }))
            .await;
        if let Err(e) = sent {
            self.pending.lock().unwrap().remove(&request_id);
            return Err(e);
        }
        match tokio::time::timeout(CONTROL_TIMEOUT, rx).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(_)) => Err(anyhow!("claude closed during control request")),
            Err(_) => {
                self.pending.lock().unwrap().remove(&request_id);
                Err(anyhow!(
                    "claude did not answer the control request within {}s",
                    CONTROL_TIMEOUT.as_secs()
                ))
            }
        }
    }

    /// Retune the resident child's model in place via `set_model`. On success,
    /// record the new model in `config` so the reuse decision stays truthful.
    async fn set_model(&self, model: &str) -> Result<()> {
        match self
            .control_request(json!({ "subtype": "set_model", "model": model }))
            .await?
        {
            Ok(_) => {
                self.config.lock().unwrap().model = Some(model.to_string());
                Ok(())
            }
            Err(msg) => Err(anyhow!("claude set_model failed: {msg}")),
        }
    }

    async fn write_line(&self, msg: &Value) -> Result<()> {
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(format!("{msg}\n").as_bytes())
            .await
            .map_err(|e| anyhow!("claude stdin: {e}"))?;
        stdin
            .flush()
            .await
            .map_err(|e| anyhow!("claude stdin: {e}"))?;
        Ok(())
    }

    /// Register the session's turn event sink. The returned guard deregisters on
    /// drop (including task abort mid-turn).
    pub fn register_turn(self: &Arc<Self>, tx: mpsc::UnboundedSender<TurnEvent>) -> TurnRoute {
        *self.turn.lock().unwrap() = Some(tx.clone());
        TurnRoute {
            client: self.clone(),
            tx,
        }
    }
}

/// RAII turn registration — dropping (normal exit or task abort) detaches the
/// event sink so a dangling turn can't receive another turn's events. An
/// aborted task's guard drops *asynchronously*, possibly after a successor turn
/// has already registered — so drop only detaches its *own* channel, never a
/// successor's (the codex `same_channel` guard).
pub struct TurnRoute {
    client: Arc<ClaudeClient>,
    tx: mpsc::UnboundedSender<TurnEvent>,
}

impl Drop for TurnRoute {
    fn drop(&mut self) {
        let mut turn = self.client.turn.lock().unwrap();
        if turn.as_ref().is_some_and(|t| t.same_channel(&self.tx)) {
            *turn = None;
        }
    }
}

/// Reader task: pump child stdout, route each line. On EOF every pending control
/// request fails and the live turn (if any) gets [`TurnEvent::Closed`].
async fn read_loop(client: Arc<ClaudeClient>, stdout: tokio::process::ChildStdout) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        match classify_line(&line) {
            ClaudeLine::ControlResponse { request_id, result } => {
                if let Some(tx) = client.pending.lock().unwrap().remove(&request_id) {
                    let _ = tx.send(result);
                }
            }
            ClaudeLine::Event(value) => {
                // Route to the live turn; dropped if none is listening (a
                // between-turns line, or an aborted turn's tail).
                let turn = client.turn.lock().unwrap();
                if let Some(tx) = turn.as_ref() {
                    let _ = tx.send(TurnEvent::Line(value));
                }
            }
            ClaudeLine::Junk => {}
        }
    }
    // EOF or read error: the connection is unusable. Kill the child if it is
    // somehow still alive (a half-dead child left in the registry would fail
    // every turn until restart), then fail everything still waiting.
    let _ = client.child.lock().await.kill().await;
    for (_, tx) in client.pending.lock().unwrap().drain() {
        let _ = tx.send(Err("claude exited".into()));
    }
    if let Some(tx) = client.turn.lock().unwrap().as_ref() {
        let _ = tx.send(TurnEvent::Closed);
    }
}

/// Spawn a resident `claude` child for one session. Mirrors the flag block the
/// old per-turn `run_turn` built (`harness/claude.rs`), plus the persistent-mode
/// wiring: `--input-format stream-json` and, on recovery/respawn,
/// `--resume <native_id>`. No handshake — the first turn's `system`/`init` line
/// is the health signal.
async fn spawn_client(spec: &SpawnSpec) -> Result<Arc<ClaudeClient>> {
    let bin = find_claude().ok_or_else(|| {
        anyhow!("claude not found on PATH — install Claude Code and run `claude` once to sign in")
    })?;
    let mut cmd = Command::new(&bin);
    cmd.args([
        "--print",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--permission-mode",
        claude_permission_mode(spec.config.permission_mode),
    ])
    .arg("--append-system-prompt-file")
    .arg(&spec.playbook)
    .current_dir(&spec.repo)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::from(crate::local::chat::harness_log("claude")?))
    // Backstop only: the child is deliberately kept resident across turns, so
    // its lifetime is managed by kill_session/shutdown, not by dropping a
    // per-turn handle. kill_on_drop still reaps it if the whole host is dropped.
    .kill_on_drop(true);
    if let Some(model) = &spec.config.model {
        cmd.args(["--model", model]);
    }
    if let Some(effort) = spec.config.effort.as_deref() {
        cmd.args(["--effort", effort]);
    }
    if let Some(native_id) = &spec.resume {
        cmd.args(["--resume", native_id]);
    }

    // Plan-mode extras, iff the child was spawned in Plan. `bridge_active` on the
    // config records what we ACHIEVED, not what we wanted: forced false here, set
    // true only on a successful bridge write. A failed write thus leaves it false
    // — the next plan turn's wanted-true mismatch respawns to retry, degrading to
    // today's no-bridge plan gating, never worse.
    let mut config = spec.config.clone();
    config.bridge_active = false;
    if spec.config.permission_mode == Some(PermissionMode::Plan) {
        match write_plan_settings(&spec.repo) {
            Ok(path) => {
                cmd.arg("--settings").arg(path);
            }
            Err(e) => {
                eprintln!(
                    "orx up: plan-mode settings not written, orx inspection will be gated: {e}"
                );
            }
        }
        // The gate token is minted HERE and ONLY here — once per child, riding
        // the mcp-gate bridge for the child's whole life. Re-minting mid-child
        // (e.g. per turn) would strand a live bridge: `request_permission`
        // equality-checks the token with no expiry (chat/mod.rs), so a fresh
        // token invalidates the resident bridge child's held requests.
        if let Some(port) = spec.chat.up_port() {
            let token = spec.chat.mint_gate_token(&spec.session_id);
            match write_mcp_config(&spec.repo, port, &spec.session_id, &token) {
                Ok(path) => {
                    cmd.arg("--mcp-config").arg(path);
                    cmd.args(["--permission-prompt-tool", "mcp__orx__approve"]);
                    // Give a held approval an hour before the CLI abandons the
                    // tool call; orx denies at 55 min, safely inside it.
                    cmd.env("MCP_TOOL_TIMEOUT", "3600000");
                    config.bridge_active = true;
                }
                Err(e) => {
                    eprintln!(
                        "orx up: mcp bridge not configured, gray-area tools will be denied: {e}"
                    );
                }
            }
        }
    }
    crate::local::chat::prepare_env(&mut cmd);
    // Own process group: a terminal SIGINT reaches orx up alone, which then
    // tears the resident child down deliberately (kill_session / shutdown). A
    // shared group would let Ctrl-C kill a persistent child mid-turn.
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("Could not spawn {}: {}", bin.display(), e))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("claude: no stdout"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("claude: no stdin"))?;

    let client = Arc::new(ClaudeClient {
        child: Mutex::new(child),
        stdin: Mutex::new(stdin),
        next_id: AtomicI64::new(1),
        pending: std::sync::Mutex::new(HashMap::new()),
        turn: std::sync::Mutex::new(None),
        config: std::sync::Mutex::new(config),
    });
    tokio::spawn(read_loop(client.clone(), stdout));
    Ok(client)
}

/// How a live child reconciles with the turn's wanted config.
///
/// * `Reuse` — identical spawn config → hand back the running child (zero cost).
/// * `SetModel` — only the model changed, to a concrete value → retune in place.
/// * `Respawn` — mode/effort/bridge changed, or the model was cleared (a launch
///   flag can't be unset live) → kill and respawn with `--resume`.
#[derive(Debug, PartialEq, Eq)]
pub enum ChildAction {
    Reuse,
    SetModel(String),
    Respawn,
}

/// Pure reuse/respawn decision (unit-tested). `current` is the running child's
/// config, `wanted` the turn's.
pub fn child_action(current: &SpawnConfig, wanted: &SpawnConfig) -> ChildAction {
    // Any launch-only axis differing forces a respawn.
    if current.permission_mode != wanted.permission_mode
        || current.effort != wanted.effort
        || current.bridge_active != wanted.bridge_active
    {
        return ChildAction::Respawn;
    }
    if current.model == wanted.model {
        return ChildAction::Reuse;
    }
    // Model differs. A change TO a concrete model retunes live; clearing the
    // model (Some → None) can't be expressed as a flag toggle, so respawn.
    match &wanted.model {
        Some(model) => ChildAction::SetModel(model.clone()),
        None => ChildAction::Respawn,
    }
}

/// The `orx up` Claude host: one resident `claude` child per chat session, keyed
/// by the orx session id. Share as `Arc<ClaudeHost>` in axum state.
pub struct ClaudeHost {
    /// Serializes ensure() spawns; `inner` is never held across a spawn.
    spawn_lock: Mutex<()>,
    inner: Mutex<HashMap<String, Arc<ClaudeClient>>>,
}

impl Default for ClaudeHost {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeHost {
    pub fn new() -> Self {
        Self {
            spawn_lock: Mutex::new(()),
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Spawn (or reuse) this session's resident child, reconciled to `spec`'s
    /// config. Idempotent while the child is alive and its config matches; a
    /// model-only change retunes live, any launch-flag change (or a dead child)
    /// respawns with `--resume`.
    ///
    /// The spawn runs in a *detached* task: the calling turn task is abortable
    /// (interrupt / delete), and an aborted future would drop its `Arc` while
    /// the reader task keeps the child alive — an unregistered client would be
    /// unreachable by every kill path and leak the process for the rest of `orx
    /// up`'s life. Detached, the bring-up always runs to completion.
    pub async fn ensure(self: &Arc<Self>, spec: SpawnSpec) -> Result<Arc<ClaudeClient>> {
        let _spawning = self.spawn_lock.lock().await;
        let session = spec.session_id.clone();
        // Reconcile against a live child under the lock.
        {
            let mut guard = self.inner.lock().await;
            if let Some(client) = guard.get(&session) {
                if matches!(client.child.lock().await.try_wait(), Ok(None)) {
                    let live = client.clone();
                    let current = live.config();
                    match child_action(&current, &spec.config) {
                        ChildAction::Reuse => return Ok(live),
                        ChildAction::SetModel(model) => {
                            drop(guard);
                            match live.set_model(&model).await {
                                Ok(()) => return Ok(live),
                                // set_model failed (transport/child error): fall
                                // through to a respawn with --resume.
                                Err(e) => {
                                    eprintln!("orx up: claude set_model failed, respawning: {e}");
                                    let _ = live.child.lock().await.kill().await;
                                    self.inner.lock().await.remove(&session);
                                }
                            }
                        }
                        ChildAction::Respawn => {
                            let _ = live.child.lock().await.kill().await;
                            guard.remove(&session);
                        }
                    }
                } else {
                    // Dead child: replace it.
                    guard.remove(&session);
                }
            }
        }
        let host = self.clone();
        tokio::spawn(async move {
            let client = spawn_client(&spec).await?;
            // Never displace a live entry: if an abandoned bring-up's insert
            // races a successor's (spawn_lock was released by the abort), the
            // loser kills its own child and defers to the live one.
            {
                let mut guard = host.inner.lock().await;
                if let Some(existing) = guard.get(&session) {
                    if matches!(existing.child.lock().await.try_wait(), Ok(None)) {
                        let existing = existing.clone();
                        drop(guard);
                        let _ = client.child.lock().await.kill().await;
                        return Ok(existing);
                    }
                }
                guard.insert(session.clone(), client.clone());
            }
            Ok(client)
        })
        .await
        .map_err(|e| anyhow!("claude bring-up task failed: {e}"))?
    }

    /// Kill and reap one session's child. The load-bearing interrupt/delete
    /// path: a resident child survives task-abort/`kill_on_drop`, so it must be
    /// killed explicitly; the next turn respawns with `--resume`.
    pub async fn kill_session(&self, session_id: &str) {
        if let Some(client) = self.inner.lock().await.remove(session_id) {
            let _ = client.child.lock().await.kill().await;
        }
    }

    /// Kill and reap every child (also happens via kill_on_drop on exit).
    pub async fn shutdown(&self) {
        for (_, client) in self.inner.lock().await.drain() {
            let _ = client.child.lock().await.kill().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(
        mode: Option<PermissionMode>,
        effort: Option<&str>,
        model: Option<&str>,
        bridge: bool,
    ) -> SpawnConfig {
        SpawnConfig {
            permission_mode: mode,
            effort: effort.map(str::to_string),
            model: model.map(str::to_string),
            bridge_active: bridge,
        }
    }

    #[test]
    fn child_action_matrix() {
        // Identical config → reuse.
        assert_eq!(
            child_action(
                &cfg(
                    Some(PermissionMode::Auto),
                    Some("high"),
                    Some("opus"),
                    false
                ),
                &cfg(
                    Some(PermissionMode::Auto),
                    Some("high"),
                    Some("opus"),
                    false
                ),
            ),
            ChildAction::Reuse
        );
        // Model-only change to a concrete value → set_model.
        assert_eq!(
            child_action(
                &cfg(
                    Some(PermissionMode::Auto),
                    Some("high"),
                    Some("opus"),
                    false
                ),
                &cfg(
                    Some(PermissionMode::Auto),
                    Some("high"),
                    Some("sonnet"),
                    false
                ),
            ),
            ChildAction::SetModel("sonnet".into())
        );
        // Clearing the model (Some → None) can't be a live flag toggle → respawn.
        assert_eq!(
            child_action(
                &cfg(
                    Some(PermissionMode::Auto),
                    Some("high"),
                    Some("opus"),
                    false
                ),
                &cfg(Some(PermissionMode::Auto), Some("high"), None, false),
            ),
            ChildAction::Respawn
        );
        // Setting a model from None is a concrete target → set_model.
        assert_eq!(
            child_action(
                &cfg(Some(PermissionMode::Auto), Some("high"), None, false),
                &cfg(
                    Some(PermissionMode::Auto),
                    Some("high"),
                    Some("opus"),
                    false
                ),
            ),
            ChildAction::SetModel("opus".into())
        );
        // Permission-mode change → respawn (launch-only).
        assert_eq!(
            child_action(
                &cfg(
                    Some(PermissionMode::Auto),
                    Some("high"),
                    Some("opus"),
                    false
                ),
                &cfg(
                    Some(PermissionMode::Plan),
                    Some("high"),
                    Some("opus"),
                    false
                ),
            ),
            ChildAction::Respawn
        );
        // Effort change → respawn (launch-only).
        assert_eq!(
            child_action(
                &cfg(
                    Some(PermissionMode::Auto),
                    Some("high"),
                    Some("opus"),
                    false
                ),
                &cfg(Some(PermissionMode::Auto), Some("max"), Some("opus"), false),
            ),
            ChildAction::Respawn
        );
        // Bridge toggling on (a plan turn that got the bridge where the child
        // didn't have it) → respawn.
        assert_eq!(
            child_action(
                &cfg(Some(PermissionMode::Plan), None, Some("opus"), false),
                &cfg(Some(PermissionMode::Plan), None, Some("opus"), true),
            ),
            ChildAction::Respawn
        );
        // Mode-and-model both differing still respawns (launch axis wins over
        // the model-only set_model shortcut).
        assert_eq!(
            child_action(
                &cfg(Some(PermissionMode::Auto), None, Some("opus"), false),
                &cfg(Some(PermissionMode::Plan), None, Some("sonnet"), false),
            ),
            ChildAction::Respawn
        );
    }

    #[test]
    fn classify_discriminates_control_responses_and_events() {
        // Success control_response carries the request_id and inner response.
        assert_eq!(
            classify_line(
                r#"{"type":"control_response","response":{"subtype":"success","request_id":"req_1","response":{"ok":true}}}"#
            ),
            ClaudeLine::ControlResponse {
                request_id: "req_1".into(),
                result: Ok(json!({"ok": true})),
            }
        );
        // Success with no inner response → Ok(Null).
        assert_eq!(
            classify_line(
                r#"{"type":"control_response","response":{"subtype":"success","request_id":"req_2"}}"#
            ),
            ClaudeLine::ControlResponse {
                request_id: "req_2".into(),
                result: Ok(Value::Null),
            }
        );
        // Error control_response surfaces the message.
        assert_eq!(
            classify_line(
                r#"{"type":"control_response","response":{"subtype":"error","request_id":"req_3","error":"nope"}}"#
            ),
            ClaudeLine::ControlResponse {
                request_id: "req_3".into(),
                result: Err("nope".into()),
            }
        );
        // A control_response without a request_id is junk, not a panic.
        assert_eq!(
            classify_line(r#"{"type":"control_response","response":{"subtype":"success"}}"#),
            ClaudeLine::Junk
        );
        // Ordinary stream-json output objects are Events.
        assert!(matches!(
            classify_line(r#"{"type":"system","subtype":"init","session_id":"abc"}"#),
            ClaudeLine::Event(_)
        ));
        assert!(matches!(
            classify_line(r#"{"type":"assistant","message":{"id":"m1","content":[]}}"#),
            ClaudeLine::Event(_)
        ));
        assert!(matches!(
            classify_line(r#"{"type":"result","subtype":"success","session_id":"abc"}"#),
            ClaudeLine::Event(_)
        ));
        // A control_request FROM the child (bridge rides HTTP, not this stream).
        assert_eq!(
            classify_line(r#"{"type":"control_request","request_id":"x","request":{}}"#),
            ClaudeLine::Junk
        );
        // Junk never panics.
        assert_eq!(classify_line("not json"), ClaudeLine::Junk);
        assert_eq!(classify_line("{}"), ClaudeLine::Junk);
    }
}
