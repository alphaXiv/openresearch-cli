//! Codex app-server host — one long-lived `codex app-server` child per chat
//! session, speaking newline-delimited JSON-RPC 2.0 over stdio (the protocol
//! the Codex TUI/IDE use; `jsonrpc` field omitted on the wire, requests flow
//! in BOTH directions — the server sends us approval requests we must answer
//! by id). Mirrors `AgentHost` (opencode.rs): spawn on demand, reap on read,
//! kill on session delete / shutdown.
//!
//! Wire shapes were pinned against codex-cli 0.144.0 via
//! `codex app-server generate-json-schema` plus a live spike (see the fixture
//! transcript in `harness/codex.rs` tests): notifications and server requests
//! carry camelCase params; approval replies are `{"decision": "accept" |
//! "acceptForSession" | "decline" | "cancel"}` (wrapped, not bare); thread ids
//! are plain UUIDs persisted as rollout files under `~/.codex/sessions`, so
//! `thread/resume {threadId}` survives an `orx up` restart.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::error::{anyhow, Result};
use crate::local::harness::codex::{ensure_orx_data_dir, find_codex};

/// One inbound line, classified. JSON-RPC over one stream: a message with both
/// `id` and `method` is a server→client *request* (approvals — must be
/// answered); `id` alone is a response to one of our requests; `method` alone
/// is a notification.
#[derive(Debug, PartialEq)]
pub enum Line {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Response {
        id: i64,
        result: Result<Value, String>,
    },
    Notification {
        method: String,
        params: Value,
    },
    Junk,
}

/// Classify one wire line. Pure — the reader task and the tests share it.
pub fn classify_line(line: &str) -> Line {
    let Ok(msg) = serde_json::from_str::<Value>(line) else {
        return Line::Junk;
    };
    let method = msg
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_string);
    let id = msg.get("id");
    match (id, method) {
        (Some(id), Some(method)) => Line::Request {
            // Echoed back verbatim in the reply — never assume integer.
            id: id.clone(),
            method,
            params: msg.get("params").cloned().unwrap_or(Value::Null),
        },
        (Some(id), None) => {
            let Some(id) = id.as_i64() else {
                return Line::Junk; // we only ever send integer ids
            };
            let result = match msg.get("error") {
                Some(err) => Err(err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("codex app-server error")
                    .to_string()),
                None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
            };
            Line::Response { id, result }
        }
        (None, Some(method)) => Line::Notification {
            method,
            params: msg.get("params").cloned().unwrap_or(Value::Null),
        },
        (None, None) => Line::Junk,
    }
}

/// One event delivered to the session's in-flight turn.
#[derive(Debug)]
pub enum TurnEvent {
    Notification {
        method: String,
        params: Value,
    },
    /// A server→client request (approval). The turn loop decides the reply
    /// (surface a card / auto-answer) and sends it via [`CodexClient::respond`].
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// Child died (EOF on stdout). The turn cannot continue.
    Closed,
}

/// The in-flight turn's ids, for `turn/interrupt`.
#[derive(Clone, Default)]
struct ActiveTurn {
    thread_id: Option<String>,
    turn_id: Option<String>,
}

/// A live JSON-RPC connection to one session's `codex app-server` child.
pub struct CodexClient {
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    next_id: AtomicI64,
    /// Our outstanding requests. Sync mutex: touched from the reader task and
    /// from `request()`, held only for map ops, never across an await.
    pending: std::sync::Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>,
    /// The session's in-flight turn, if any (one per session-child). The
    /// registration guard drops synchronously on task abort, hence sync mutex.
    turn: std::sync::Mutex<Option<mpsc::UnboundedSender<TurnEvent>>>,
    /// Ids of server→client requests we have not answered yet, as raw JSON
    /// text (ids are echoed verbatim). Guards `respond` against stale answers.
    unanswered: std::sync::Mutex<HashMap<String, ()>>,
    active: std::sync::Mutex<ActiveTurn>,
    /// The thread this child has started/resumed — a fresh child (after crash
    /// or restart) must `thread/resume` before its next `turn/start`.
    resumed_thread: std::sync::Mutex<Option<String>>,
}

impl CodexClient {
    /// Send a client→server request and await its response.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let sent = self
            .write_line(&json!({ "id": id, "method": method, "params": params }))
            .await;
        if let Err(e) = sent {
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        // Generous ceiling: thread/start can wait on the user's MCP servers.
        match tokio::time::timeout(Duration::from_secs(150), rx).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(err))) => Err(anyhow!("codex {method} failed: {err}")),
            Ok(Err(_)) => Err(anyhow!("codex app-server closed during {method}")),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(anyhow!("codex app-server did not answer {method}"))
            }
        }
    }

    /// Send a notification (no id, no reply).
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<()> {
        let mut msg = json!({ "method": method });
        if let Some(params) = params {
            msg["params"] = params;
        }
        self.write_line(&msg).await
    }

    /// Answer a server→client request (approval). Errors if `id` isn't
    /// pending — the stale-answer guard (child restarted, turn ended).
    pub async fn respond(&self, id: &Value, result: Value) -> Result<()> {
        if self
            .unanswered
            .lock()
            .unwrap()
            .remove(&id.to_string())
            .is_none()
        {
            return Err(anyhow!("this approval is no longer pending"));
        }
        self.write_line(&json!({ "id": id, "result": result }))
            .await
    }

    async fn write_line(&self, msg: &Value) -> Result<()> {
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(format!("{msg}\n").as_bytes())
            .await
            .map_err(|e| anyhow!("codex app-server stdin: {e}"))?;
        stdin
            .flush()
            .await
            .map_err(|e| anyhow!("codex app-server stdin: {e}"))?;
        Ok(())
    }

    /// Register the session's turn event sink. The returned guard deregisters
    /// on drop (including task abort mid-turn).
    pub fn register_turn(self: &Arc<Self>, tx: mpsc::UnboundedSender<TurnEvent>) -> TurnRoute {
        *self.turn.lock().unwrap() = Some(tx);
        TurnRoute {
            client: self.clone(),
        }
    }

    /// The thread this child has already started/resumed, if any.
    pub fn resumed_thread(&self) -> Option<String> {
        self.resumed_thread.lock().unwrap().clone()
    }

    pub fn set_resumed_thread(&self, thread_id: &str) {
        *self.resumed_thread.lock().unwrap() = Some(thread_id.to_string());
        self.active.lock().unwrap().thread_id = Some(thread_id.to_string());
    }

    pub fn set_active_turn(&self, turn_id: &str) {
        self.active.lock().unwrap().turn_id = Some(turn_id.to_string());
    }

    /// Best-effort native interrupt of the in-flight turn.
    pub async fn interrupt_active_turn(&self) {
        let ActiveTurn { thread_id, turn_id } = self.active.lock().unwrap().clone();
        if let (Some(thread_id), Some(turn_id)) = (thread_id, turn_id) {
            let _ = self
                .request(
                    "turn/interrupt",
                    json!({ "threadId": thread_id, "turnId": turn_id }),
                )
                .await;
        }
    }
}

/// RAII turn registration — dropping (normal exit or task abort) detaches the
/// event sink so a dangling turn can't receive another turn's events.
pub struct TurnRoute {
    client: Arc<CodexClient>,
}

impl Drop for TurnRoute {
    fn drop(&mut self) {
        *self.client.turn.lock().unwrap() = None;
        self.client.active.lock().unwrap().turn_id = None;
    }
}

/// Reader task: pump child stdout, route each line. On EOF every pending
/// request fails and the live turn (if any) gets [`TurnEvent::Closed`].
async fn read_loop(client: Arc<CodexClient>, stdout: tokio::process::ChildStdout) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        match classify_line(&line) {
            Line::Response { id, result } => {
                if let Some(tx) = client.pending.lock().unwrap().remove(&id) {
                    let _ = tx.send(result);
                }
            }
            Line::Request { id, method, params } => {
                client.unanswered.lock().unwrap().insert(id.to_string(), ());
                let routed = {
                    let turn = client.turn.lock().unwrap();
                    turn.as_ref()
                        .map(|tx| {
                            tx.send(TurnEvent::Request {
                                id: id.clone(),
                                method: method.clone(),
                                params: params.clone(),
                            })
                            .is_ok()
                        })
                        .unwrap_or(false)
                };
                if !routed {
                    // No turn is listening (raced an abort). Never leave the
                    // server hanging on a request nobody will answer.
                    let _ = client.respond(&id, json!({ "decision": "decline" })).await;
                }
            }
            Line::Notification { method, params } => {
                let turn = client.turn.lock().unwrap();
                if let Some(tx) = turn.as_ref() {
                    let _ = tx.send(TurnEvent::Notification { method, params });
                }
            }
            Line::Junk => {}
        }
    }
    // EOF: the child is gone. Fail everything that is still waiting.
    for (_, tx) in client.pending.lock().unwrap().drain() {
        let _ = tx.send(Err("codex app-server exited".into()));
    }
    client.unanswered.lock().unwrap().clear();
    if let Some(tx) = client.turn.lock().unwrap().as_ref() {
        let _ = tx.send(TurnEvent::Closed);
    }
}

/// Spawn `codex app-server` and complete the `initialize` handshake.
async fn spawn_client() -> Result<Arc<CodexClient>> {
    let bin = find_codex().ok_or_else(|| {
        anyhow!("codex not found on PATH — install Codex and run `codex login` first")
    })?;
    let mut cmd = Command::new(&bin);
    cmd.arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(crate::local::chat::harness_log("codex")?))
        .kill_on_drop(true);
    crate::local::chat::prepare_env(&mut cmd);
    // Pin the child's store to the same canonicalized dir the sandbox policy
    // grants (see harness/codex.rs `ensure_orx_data_dir`) — after prepare_env
    // so the pin beats a dashboard-synced ORX_DATA_DIR. Unconditional: the
    // child is shared by every permission mode, so unlike the old per-turn
    // exec pin this also applies under Bypass (more coherent — agent store ==
    // served store in every mode).
    if let Some(dir) = ensure_orx_data_dir() {
        cmd.env("ORX_DATA_DIR", &dir);
    }
    // The sandbox blocks the keyring `gh` keeps its token in; resolve it out
    // here and pass it down. Resolved once per child, not per turn.
    if let Some(token) = crate::local::git::resolve_github_token() {
        cmd.env("GH_TOKEN", &token);
        cmd.env("GITHUB_TOKEN", token);
    }
    // Own process group: a terminal SIGINT reaches orx up alone, which then
    // tears the child down deliberately (kill_on_drop / shutdown()).
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("Could not spawn {} app-server: {}", bin.display(), e))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("codex app-server: no stdout"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("codex app-server: no stdin"))?;

    let client = Arc::new(CodexClient {
        child: Mutex::new(child),
        stdin: Mutex::new(stdin),
        next_id: AtomicI64::new(1),
        pending: std::sync::Mutex::new(HashMap::new()),
        turn: std::sync::Mutex::new(None),
        unanswered: std::sync::Mutex::new(HashMap::new()),
        active: std::sync::Mutex::new(ActiveTurn::default()),
        resumed_thread: std::sync::Mutex::new(None),
    });
    tokio::spawn(read_loop(client.clone(), stdout));

    let handshake = client.request(
        "initialize",
        json!({ "clientInfo": {
            "name": "orx",
            "title": "OpenResearch",
            "version": env!("CARGO_PKG_VERSION"),
        }}),
    );
    match tokio::time::timeout(Duration::from_secs(10), handshake).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            let _ = client.child.lock().await.kill().await;
            return Err(e);
        }
        Err(_) => {
            let _ = client.child.lock().await.kill().await;
            return Err(anyhow!(
                "codex app-server did not answer initialize within 10s; see {}",
                crate::store::data_dir().join("agent-codex.log").display()
            ));
        }
    }
    client.notify("initialized", None).await?;
    Ok(client)
}

/// The `orx up` codex host: one `codex app-server` child per chat session,
/// keyed by the orx session id. Share as `Arc<CodexHost>` in axum state.
pub struct CodexHost {
    /// Serializes ensure() spawns (a spawn is quick, and one at a time keeps
    /// the handshake traffic sane). `inner` is never held across a spawn.
    spawn_lock: Mutex<()>,
    inner: Mutex<HashMap<String, Arc<CodexClient>>>,
}

impl Default for CodexHost {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexHost {
    pub fn new() -> Self {
        Self {
            spawn_lock: Mutex::new(()),
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Spawn (or reuse) this session's app-server child. Idempotent while the
    /// child is alive; a dead child is replaced (its thread is re-resumed by
    /// the caller — `resumed_thread` starts empty on the replacement).
    pub async fn ensure(&self, session_id: &str) -> Result<Arc<CodexClient>> {
        let _spawning = self.spawn_lock.lock().await;
        {
            let mut guard = self.inner.lock().await;
            if let Some(client) = guard.get(session_id) {
                if matches!(client.child.lock().await.try_wait(), Ok(None)) {
                    return Ok(client.clone());
                }
                guard.remove(session_id);
            }
        }
        let client = spawn_client().await?;
        self.inner
            .lock()
            .await
            .insert(session_id.to_string(), client.clone());
        Ok(client)
    }

    /// The session's live client, if any (for inline replies / interrupts).
    pub async fn client_for(&self, session_id: &str) -> Option<Arc<CodexClient>> {
        let mut guard = self.inner.lock().await;
        let client = guard.get(session_id)?;
        if matches!(client.child.lock().await.try_wait(), Ok(None)) {
            Some(client.clone())
        } else {
            guard.remove(session_id);
            None
        }
    }

    /// Natively interrupt the session's in-flight turn (best-effort).
    pub async fn interrupt_session(&self, session_id: &str) {
        if let Some(client) = self.client_for(session_id).await {
            client.interrupt_active_turn().await;
        }
    }

    /// Kill and reap one session's child (on session delete).
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

    #[test]
    fn classify_discriminates_the_three_wire_shapes() {
        // Server→client request: id + method. Id kept as raw Value.
        assert_eq!(
            classify_line(
                r#"{"id":0,"method":"item/commandExecution/requestApproval","params":{"threadId":"t"}}"#
            ),
            Line::Request {
                id: json!(0),
                method: "item/commandExecution/requestApproval".into(),
                params: json!({"threadId": "t"}),
            }
        );
        // Response to one of our requests: id only.
        assert_eq!(
            classify_line(r#"{"id":2,"result":{"thread":{"id":"abc"}}}"#),
            Line::Response {
                id: 2,
                result: Ok(json!({"thread":{"id":"abc"}}))
            }
        );
        // Error response carries the message.
        assert_eq!(
            classify_line(r#"{"id":3,"error":{"code":-32600,"message":"bad"}}"#),
            Line::Response {
                id: 3,
                result: Err("bad".into())
            }
        );
        // Notification: method only.
        assert_eq!(
            classify_line(r#"{"method":"turn/completed","params":{"threadId":"t"}}"#),
            Line::Notification {
                method: "turn/completed".into(),
                params: json!({"threadId":"t"})
            }
        );
        // Junk never panics.
        assert_eq!(classify_line("not json"), Line::Junk);
        assert_eq!(classify_line("{}"), Line::Junk);
        // Non-integer response id (we only send integers) is junk, not a panic.
        assert_eq!(classify_line(r#"{"id":"weird","result":{}}"#), Line::Junk);
    }
}
