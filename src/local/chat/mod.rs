//! Unified chat layer for `orx up` — one session/message model over three
//! harness adapters (Claude Code, Codex, OpenCode), each a local child
//! process using the user's own login. orx's SQLite is the system of record
//! for transcripts; each harness keeps its native session for context/resume.
//!
//! Flow: `POST /api/chat/sessions/{id}/message` → `ChatHost::send_message`
//! persists the user message and spawns one turn task. The adapter streams
//! normalized parts into the per-turn assistant message; every flush persists
//! the message and broadcasts it as a `chat.message` SSE event.

mod claude;
mod codex;
mod opencode;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{broadcast, Mutex};

use crate::error::{anyhow, Result};
use crate::local::model::LocalProject;
use crate::local::opencode::AgentHost;
use crate::store::{now_ms, Store, StoredChatMessage, StoredChatSession};

pub const HARNESS_IDS: [&str; 3] = ["claude-code", "codex", "opencode"];

/// Min interval between mid-turn persist+broadcast flushes (streaming parts
/// can update many times a second; the final flush is always unconditional).
const FLUSH_INTERVAL: Duration = Duration::from_millis(150);

// --- wire types (what the UI renders) ---------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireToolState {
    pub status: String, // running | completed | error
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WirePart {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String, // text | reasoning | tool
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<WireToolState>,
}

impl WirePart {
    pub fn text(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind: "text".into(),
            text: Some(text.into()),
            tool: None,
            state: None,
        }
    }

    pub fn reasoning(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            kind: "reasoning".into(),
            ..Self::text(id, text)
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WireMessage {
    pub id: String,
    pub role: String,
    pub parts: Vec<WirePart>,
    pub created_at: i64,
}

pub fn session_json(s: &StoredChatSession, busy: bool) -> Value {
    json!({
        "id": s.id,
        "projectId": s.project_id,
        "harness": s.harness,
        "title": s.title,
        "model": s.model,
        "createdAt": s.created_at,
        "updatedAt": s.updated_at,
        "busy": busy,
    })
}

fn message_json(m: &WireMessage, session_id: &str) -> Value {
    json!({ "sessionId": session_id, "message": m })
}

fn stored_to_wire(m: &StoredChatMessage) -> WireMessage {
    WireMessage {
        id: m.id.clone(),
        role: m.role.clone(),
        parts: serde_json::from_str(&m.parts_json).unwrap_or_default(),
        created_at: m.created_at,
    }
}

// --- host --------------------------------------------------------------------

/// Owns turn tasks and the chat event stream. One per `orx up` process.
pub struct ChatHost {
    /// Lazy opencode serve manager (only the opencode adapter spawns it).
    pub opencode: Arc<AgentHost>,
    http: reqwest::Client,
    events: broadcast::Sender<(&'static str, Value)>,
    turns: Mutex<HashMap<String, tokio::task::AbortHandle>>,
}

impl ChatHost {
    pub fn new(opencode: Arc<AgentHost>) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            opencode,
            http: reqwest::Client::new(),
            events,
            turns: Mutex::new(HashMap::new()),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<(&'static str, Value)> {
        self.events.subscribe()
    }

    fn emit(&self, name: &'static str, data: Value) {
        let _ = self.events.send((name, data));
    }

    pub async fn busy_sessions(&self) -> Vec<String> {
        self.turns.lock().await.keys().cloned().collect()
    }

    pub async fn is_busy(&self, session_id: &str) -> bool {
        self.turns.lock().await.contains_key(session_id)
    }

    /// Persist the user message and run one harness turn in the background.
    pub async fn send_message(
        self: &Arc<Self>,
        session_id: &str,
        text: String,
        model_override: Option<String>,
    ) -> Result<()> {
        if self.is_busy(session_id).await {
            return Err(anyhow!("session is busy — interrupt it first"));
        }
        let store = Store::open()?;
        let mut session = store
            .get_chat_session(session_id)?
            .ok_or_else(|| anyhow!("chat session not found"))?;
        let project = store
            .get_local_project(&session.project_id)?
            .ok_or_else(|| anyhow!("project not found"))?;

        if let Some(model) = model_override.filter(|m| !m.is_empty()) {
            if session.model.as_deref() != Some(model.as_str()) {
                store.set_chat_session_model(&session.id, &model)?;
                session.model = Some(model);
            }
        }
        if session.title.is_none() {
            let title: String = text.lines().next().unwrap_or("").chars().take(64).collect();
            let title = title.trim().to_string();
            if !title.is_empty() {
                store.set_chat_session_title(&session.id, &title)?;
                session.title = Some(title);
            }
        }

        let user_msg = WireMessage {
            id: format!("msg_{}", uuid::Uuid::new_v4()),
            role: "user".into(),
            parts: vec![WirePart::text("p0", text.clone())],
            created_at: now_ms(),
        };
        store.upsert_chat_message(&StoredChatMessage {
            id: user_msg.id.clone(),
            session_id: session.id.clone(),
            role: "user".into(),
            parts_json: serde_json::to_string(&user_msg.parts)?,
            created_at: user_msg.created_at,
        })?;
        store.touch_chat_session(&session.id)?;
        let session = store.get_chat_session(&session.id)?.unwrap_or(session);

        self.emit("chat.message", message_json(&user_msg, &session.id));
        self.emit(
            "chat.session",
            json!({ "session": session_json(&session, true) }),
        );
        self.emit(
            "chat.busy",
            json!({ "sessionId": session.id, "busy": true }),
        );

        let sid = session.id.clone();
        let mut ctx = TurnCtx {
            host: self.clone(),
            session_id: session.id.clone(),
            harness: session.harness.clone(),
            native_session_id: session.native_session_id.clone(),
            model: session.model.clone(),
            project,
            text,
            assistant: WireMessage {
                id: format!("msg_{}", uuid::Uuid::new_v4()),
                role: "assistant".into(),
                parts: Vec::new(),
                created_at: now_ms(),
            },
            last_flush: Instant::now() - FLUSH_INTERVAL,
        };
        let task = tokio::spawn(async move {
            let result = match ctx.harness.as_str() {
                "claude-code" => claude::run_turn(&mut ctx).await,
                "codex" => codex::run_turn(&mut ctx).await,
                "opencode" => opencode::run_turn(&mut ctx).await,
                other => Err(anyhow!("unknown harness: {other}")),
            };
            if let Err(err) = result {
                ctx.push_error(format!("{err}"));
            }
            let _ = ctx.flush();
            ctx.host.finish_turn(&ctx.session_id).await;
        });
        self.turns.lock().await.insert(sid, task.abort_handle());
        Ok(())
    }

    /// Turn cleanup: drop the handle, bump the session, broadcast idle.
    async fn finish_turn(&self, session_id: &str) {
        self.turns.lock().await.remove(session_id);
        if let Ok(store) = Store::open() {
            let _ = store.touch_chat_session(session_id);
            if let Ok(Some(session)) = store.get_chat_session(session_id) {
                self.emit(
                    "chat.session",
                    json!({ "session": session_json(&session, false) }),
                );
            }
        }
        self.emit(
            "chat.busy",
            json!({ "sessionId": session_id, "busy": false }),
        );
    }

    /// Abort an in-flight turn. Child processes die via kill_on_drop; the
    /// opencode adapter additionally gets a native abort so the serve process
    /// stops generating.
    pub async fn interrupt(&self, session_id: &str) -> Result<()> {
        let handle = self.turns.lock().await.remove(session_id);
        let Some(handle) = handle else {
            return Ok(());
        };
        if let Ok(store) = Store::open() {
            if let Ok(Some(session)) = store.get_chat_session(session_id) {
                if session.harness == "opencode" {
                    if let (Some(nid), Some(port)) =
                        (&session.native_session_id, self.opencode.proxy_port().await)
                    {
                        let url = format!("http://127.0.0.1:{port}/session/{nid}/abort");
                        let _ = self.http.post(url).body("{}").send().await;
                    }
                }
            }
        }
        handle.abort();
        self.finish_turn(session_id).await;
        Ok(())
    }

    pub async fn delete_session(&self, session_id: &str) -> Result<()> {
        let _ = self.interrupt(session_id).await;
        Store::open()?.delete_chat_session(session_id)?;
        Ok(())
    }
}

// --- per-turn context handed to adapters --------------------------------------

pub struct TurnCtx {
    pub host: Arc<ChatHost>,
    pub session_id: String,
    pub harness: String,
    pub native_session_id: Option<String>,
    pub model: Option<String>,
    pub project: LocalProject,
    pub text: String,
    pub assistant: WireMessage,
    last_flush: Instant,
}

impl TurnCtx {
    pub fn http(&self) -> &reqwest::Client {
        &self.host.http
    }

    /// Record the harness's own session id (CLIs mint/rotate them per turn).
    pub fn set_native_session_id(&mut self, native_id: &str) {
        if self.native_session_id.as_deref() == Some(native_id) {
            return;
        }
        self.native_session_id = Some(native_id.to_string());
        if let Ok(store) = Store::open() {
            let _ = store.set_chat_session_native_id(&self.session_id, native_id);
        }
    }

    pub fn set_title(&self, title: &str) {
        let title = title.trim();
        if title.is_empty() {
            return;
        }
        if let Ok(store) = Store::open() {
            let _ = store.set_chat_session_title(&self.session_id, title);
            if let Ok(Some(session)) = store.get_chat_session(&self.session_id) {
                self.host.emit(
                    "chat.session",
                    json!({ "session": session_json(&session, true) }),
                );
            }
        }
    }

    /// Insert or replace a part by id, preserving arrival order.
    pub fn upsert_part(&mut self, part: WirePart) {
        match self.assistant.parts.iter_mut().find(|p| p.id == part.id) {
            Some(existing) => *existing = part,
            None => self.assistant.parts.push(part),
        }
    }

    pub fn append_part_text(&mut self, part_id: &str, delta: &str) {
        if let Some(part) = self.assistant.parts.iter_mut().find(|p| p.id == part_id) {
            let text = part.text.get_or_insert_with(String::new);
            text.push_str(delta);
        }
    }

    pub fn push_error(&mut self, message: String) {
        let id = format!("err-{}", self.assistant.parts.len());
        self.assistant.parts.push(WirePart {
            id,
            kind: "tool".into(),
            text: None,
            tool: Some("error".into()),
            state: Some(WireToolState {
                status: "error".into(),
                input: None,
                output: None,
                error: Some(message),
                title: None,
            }),
        });
    }

    /// Persist + broadcast the assistant message, rate-limited mid-turn.
    pub fn maybe_flush(&mut self) {
        if self.last_flush.elapsed() >= FLUSH_INTERVAL {
            let _ = self.flush();
        }
    }

    pub fn flush(&mut self) -> Result<()> {
        self.last_flush = Instant::now();
        if self.assistant.parts.is_empty() {
            return Ok(());
        }
        let store = Store::open()?;
        store.upsert_chat_message(&StoredChatMessage {
            id: self.assistant.id.clone(),
            session_id: self.session_id.clone(),
            role: "assistant".into(),
            parts_json: serde_json::to_string(&self.assistant.parts)?,
            created_at: self.assistant.created_at,
        })?;
        self.host.emit(
            "chat.message",
            message_json(&self.assistant, &self.session_id),
        );
        Ok(())
    }
}

// --- shared adapter helpers ----------------------------------------------------

/// Transcript replay for the UI.
pub fn list_messages(session_id: &str) -> Result<Vec<WireMessage>> {
    let store = Store::open()?;
    Ok(store
        .list_chat_messages(session_id)?
        .iter()
        .map(stored_to_wire)
        .collect())
}

// --- run watcher ----------------------------------------------------------------

fn is_terminal(status: &str) -> bool {
    matches!(status, "done" | "failed" | "cancelled")
}

/// Poke a project's chat when a run completes while no turn is in flight —
/// the local stand-in for the cloud agent staying online inside a blocking
/// `orx exp wait`. The first pass only seeds the cursor, so a server restart
/// doesn't replay old completions. Busy sessions are skipped (the agent is
/// awake — likely in its wait loop — and will see the completion itself).
pub async fn watch_runs(chat: Arc<ChatHost>) {
    let mut seen: HashMap<String, String> = HashMap::new();
    let mut first = true;
    loop {
        tokio::time::sleep(Duration::from_secs(3)).await;
        // Store hiccups (locked db) just skip a tick.
        let Ok(store) = Store::open() else { continue };
        let Ok(runs) = store.list_runs(200) else { continue };
        for run in runs {
            let prev = seen.insert(run.id.clone(), run.status.clone());
            let newly_terminal = is_terminal(&run.status)
                && !matches!(prev.as_deref(), Some(s) if is_terminal(s));
            if first || !newly_terminal {
                continue;
            }
            let Ok(sessions) = store.list_chat_sessions_by_project(&run.project_id) else {
                continue;
            };
            // Most recently touched session that already has history — never
            // mint or retitle a fresh one.
            let Some(session) = sessions.into_iter().find(|s| {
                store
                    .list_chat_messages(&s.id)
                    .map(|m| !m.is_empty())
                    .unwrap_or(false)
            }) else {
                continue;
            };
            if chat.is_busy(&session.id).await {
                continue;
            }
            let text = format!(
                "[orx] Run `{}` finished with status **{}**. Reconcile with \
                 `orx runs {}`, analyze the result (`orx logs {}`), and \
                 continue the loop.",
                run.id, run.status, run.project_id, run.id
            );
            if let Err(err) = chat.send_message(&session.id, text, None).await {
                eprintln!("orx up: run watcher: {err}");
            }
        }
        first = false;
    }
}

/// Env prep shared by the CLI adapters: this orx first on PATH (agents shell
/// out to `orx`) and the dashboard-managed env vars, real env winning.
pub fn prepare_env(cmd: &mut tokio::process::Command) {
    if let Ok(exe) = std::env::current_exe().and_then(|p| p.canonicalize()) {
        if let Some(dir) = exe.parent() {
            let mut path = std::ffi::OsString::from(dir);
            if let Some(existing) = std::env::var_os("PATH").filter(|p| !p.is_empty()) {
                path.push(":");
                path.push(existing);
            }
            cmd.env("PATH", path);
        }
    }
    for (key, value) in crate::config::list_synced_env() {
        if std::env::var_os(&key).is_none() {
            cmd.env(key, value);
        }
    }
}

/// Append-only stderr sink for a harness child (startup/debug diagnostics).
pub fn harness_log(name: &str) -> Result<std::fs::File> {
    let path = crate::store::data_dir().join(format!("agent-{name}.log"));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow!("Could not create {}: {}", parent.display(), e))?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| anyhow!("Could not open {}: {}", path.display(), e))
}
