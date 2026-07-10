//! Unified chat layer for `orx up` — one session/message model over three
//! harness adapters (Claude Code, Codex, OpenCode), each a local child
//! process using the user's own login. orx's SQLite is the system of record
//! for transcripts; each harness keeps its native session for context/resume.
//!
//! Flow: `POST /api/chat/sessions/{id}/message` → `ChatHost::send_message`
//! persists the user message and spawns one turn task. The adapter streams
//! normalized parts into the per-turn assistant message; every flush persists
//! the message and broadcasts it as a `chat.message` SSE event.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{broadcast, Mutex};

use crate::error::{anyhow, Result};
use crate::local::harness::ResumeAction;
use crate::local::model::LocalProject;
use crate::local::opencode::AgentHost;
use crate::store::{now_ms, Store, StoredChatMessage, StoredChatSession};

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

/// One option in an AskUserQuestion prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireQuestionOption {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// An interactive request the user must act on before the harness continues.
/// The three kinds (`plan` / `permission` / `question`) originated with Claude
/// Code's ExitPlanMode / permission_denials / AskUserQuestion, but `permission`
/// and `question` are now shared: OpenCode emits them from its serve
/// `permission.asked` / `question.asked` events (see `harness/opencode.rs`).
/// `plan` remains Claude-only.
///
/// How the answer flows back is per-harness (see [`crate::local::harness::ResumeAction`]):
/// Claude ends its turn and resumes with a new message; OpenCode is paused
/// mid-turn and the answer is replied inline over the live serve session — which
/// is what `native_id` is for. The UI renders a card either way.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WirePrompt {
    /// `plan` | `permission` | `question`.
    pub kind: String,
    /// Whether this prompt has been answered (answered cards render read-only).
    #[serde(default)]
    pub resolved: bool,
    /// plan: the proposed plan markdown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// permission: the tool the harness was blocked from using, + its input.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<Value>,
    /// question: the prompt text + selectable options.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub options: Vec<WireQuestionOption>,
    #[serde(default)]
    pub multi_select: bool,
    /// The harness-native id used to reply over a live protocol (opencode's
    /// permission/question request id). Internal to the backend resume path —
    /// the UI never reads it and only echoes the `WirePart` id. `None` for
    /// end-turn harnesses (Claude), which resume by message, not by reply id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WirePart {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String, // text | reasoning | tool | prompt
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<WireToolState>,
    /// Present only on `prompt` parts — the interactive request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<WirePrompt>,
}

impl WirePart {
    pub fn text(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind: "text".into(),
            text: Some(text.into()),
            tool: None,
            state: None,
            prompt: None,
        }
    }

    pub fn reasoning(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            kind: "reasoning".into(),
            ..Self::text(id, text)
        }
    }

    /// `text` holds the attachment file name (served via /api/chat/attachments).
    pub fn image(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            kind: "image".into(),
            ..Self::text(id, name)
        }
    }

    /// An interactive prompt part (plan / permission / question).
    pub fn prompt(id: impl Into<String>, prompt: WirePrompt) -> Self {
        Self {
            id: id.into(),
            kind: "prompt".into(),
            text: None,
            tool: None,
            state: None,
            prompt: Some(prompt),
        }
    }
}

// --- image attachments ---------------------------------------------------------

/// Pasted image riding the send-message request.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageAttachment {
    pub media_type: String,
    pub data_base64: String,
}

pub fn attachments_dir() -> Result<std::path::PathBuf> {
    let dir = crate::store::data_dir().join("chat-attachments");
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow!("Could not create {}: {}", dir.display(), e))?;
    Ok(dir)
}

fn image_ext(media_type: &str) -> Option<&'static str> {
    match media_type {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

pub fn attachment_content_type(name: &str) -> &'static str {
    match name.rsplit('.').next() {
        Some("png") => "image/png",
        Some("jpg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "application/octet-stream",
    }
}

/// Decode pasted images to the attachments dir; returns (file name, abs path).
fn save_images(images: &[ImageAttachment]) -> Result<Vec<(String, std::path::PathBuf)>> {
    if images.is_empty() {
        return Ok(Vec::new());
    }
    let dir = attachments_dir()?;
    let mut saved = Vec::new();
    for img in images {
        let ext = image_ext(&img.media_type)
            .ok_or_else(|| anyhow!("unsupported image type: {}", img.media_type))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(img.data_base64.as_bytes())
            .map_err(|e| anyhow!("bad image data: {e}"))?;
        let name = format!("img_{}.{ext}", uuid::Uuid::new_v4());
        let path = dir.join(&name);
        std::fs::write(&path, bytes)
            .map_err(|e| anyhow!("Could not write {}: {}", path.display(), e))?;
        saved.push((name, path));
    }
    Ok(saved)
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
        "permissionMode": s.permission_mode,
        "reasoningLevel": s.reasoning_level,
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
    /// Sessions with a turn in flight. A key present means busy; the value is
    /// the running task's abort handle, or `None` while a turn is being set up
    /// (reserved but not yet spawned — see `TurnGuard`).
    turns: Mutex<HashMap<String, Option<tokio::task::AbortHandle>>>,
    /// Per-session serialization for `respond`. Answering a prompt reads the
    /// card, delivers the answer (a non-idempotent POST for inline harnesses),
    /// and marks it resolved — steps that must not interleave for one session,
    /// or a double-submit could fire the reply twice. Held only for the brief
    /// `respond` critical section; keyed per session so different sessions don't
    /// contend. (The busy `turns` slot can't gate this: an inline harness is
    /// *deliberately* busy while paused on the prompt.)
    respond_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Guards the read-modify-write of a chat message's `parts_json` blob so two
    /// writers can't lost-update each other. The dangerous pair: a still-running
    /// opencode turn's `flush` (which carries a concurrently-resolved card's flag
    /// forward via `adopt_resolved_prompts`) vs `respond`'s `mark_prompt_resolved`
    /// — both do read→modify→write on the *same* message, and SQLite's WAL
    /// serializes the writes but not the logical transaction. A single process-
    /// wide sync mutex (writes are brief and already WAL-serialized, so this adds
    /// no real contention) makes each RMW atomic. Sync because `flush` is sync;
    /// never held across an `.await`.
    msg_write: std::sync::Mutex<()>,
}

/// Reserves a session's turn slot for the duration of `send_message`'s setup.
/// `claim` inserts a `None` reservation under the `turns` lock iff the session
/// isn't already busy — closing the check-then-insert race. On drop (early
/// error / panic) it clears the reservation; call `defuse` once the real abort
/// handle has replaced it so the running turn's slot survives.
struct TurnGuard {
    host: Arc<ChatHost>,
    session_id: String,
    armed: bool,
}

impl TurnGuard {
    /// `Some` if the slot was free and is now reserved; `None` if already busy.
    async fn claim(host: &Arc<ChatHost>, session_id: &str) -> Option<Self> {
        let mut turns = host.turns.lock().await;
        if turns.contains_key(session_id) {
            return None;
        }
        turns.insert(session_id.to_string(), None);
        Some(Self {
            host: host.clone(),
            session_id: session_id.to_string(),
            armed: true,
        })
    }

    /// Hand ownership of the slot to the spawned turn — stop clearing it on drop.
    fn defuse(mut self) {
        self.armed = false;
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Setup failed before a turn was spawned: release the reservation. Only
        // remove if it's still the unspawned reservation (None), never a live
        // handle (some other path may have taken over).
        //
        // `try_lock` is safe here rather than a leak risk: an armed guard only
        // drops on an early return from send_message's prologue, which never
        // holds the `turns` lock (claim releases it immediately, and it's only
        // re-acquired at the final upgrade after the guard is defused). So the
        // lock is always free when an armed guard drops — the fallible lock can't
        // actually fail in this path.
        if let Ok(mut turns) = self.host.turns.try_lock() {
            if matches!(turns.get(&self.session_id), Some(None)) {
                turns.remove(&self.session_id);
            }
        }
    }
}

impl ChatHost {
    pub fn new(opencode: Arc<AgentHost>) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            opencode,
            http: reqwest::Client::new(),
            events,
            turns: Mutex::new(HashMap::new()),
            respond_locks: Mutex::new(HashMap::new()),
            msg_write: std::sync::Mutex::new(()),
        }
    }

    /// The per-session `respond` lock, created on first use. The map only grows
    /// (one small `Arc<Mutex>` per session ever answered) — negligible for a
    /// single `orx up` process's session count.
    async fn respond_lock(&self, session_id: &str) -> Arc<Mutex<()>> {
        self.respond_locks
            .lock()
            .await
            .entry(session_id.to_string())
            .or_default()
            .clone()
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
        overrides: TurnOverrides,
        images: Vec<ImageAttachment>,
    ) -> Result<()> {
        // Atomically claim the session's turn slot: the busy-check and the
        // reservation happen under one lock so two concurrent sends (or a
        // send racing a /respond resume) can't both spawn a turn against the
        // same session. `_guard` releases the reservation on any early error.
        let _guard = match TurnGuard::claim(self, session_id).await {
            Some(guard) => guard,
            None => return Err(anyhow!("session is busy — interrupt it first")),
        };
        let store = Store::open()?;
        let mut session = store
            .get_chat_session(session_id)?
            .ok_or_else(|| anyhow!("chat session not found"))?;
        let project = store
            .get_local_project(&session.project_id)?
            .ok_or_else(|| anyhow!("project not found"))?;

        // Composer selections are sticky: an override that differs from the
        // stored value is persisted so the next turn (and a reload) keep it.
        if let Some(model) = overrides.model.filter(|m| !m.is_empty()) {
            if session.model.as_deref() != Some(model.as_str()) {
                store.set_chat_session_model(&session.id, &model)?;
                session.model = Some(model);
            }
        }
        if let Some(mode) = overrides.permission_mode.filter(|m| !m.is_empty()) {
            if session.permission_mode.as_deref() != Some(mode.as_str()) {
                store.set_chat_session_permission_mode(&session.id, &mode)?;
                session.permission_mode = Some(mode);
            }
        }
        if let Some(level) = overrides.reasoning_level.filter(|l| !l.is_empty()) {
            if session.reasoning_level.as_deref() != Some(level.as_str()) {
                store.set_chat_session_reasoning_level(&session.id, &level)?;
                session.reasoning_level = Some(level);
            }
        }
        let saved_images = save_images(&images)?;
        if session.title.is_none() {
            let title: String = text.lines().next().unwrap_or("").chars().take(64).collect();
            let mut title = title.trim().to_string();
            if title.is_empty() && !saved_images.is_empty() {
                title = "Image".into();
            }
            if !title.is_empty() {
                store.set_chat_session_title(&session.id, &title)?;
                session.title = Some(title);
            }
        }

        let mut parts = Vec::new();
        if !text.is_empty() {
            parts.push(WirePart::text("p0", text.clone()));
        }
        for (i, (name, _)) in saved_images.iter().enumerate() {
            parts.push(WirePart::image(format!("img{i}"), name.clone()));
        }
        let user_msg = WireMessage {
            id: format!("msg_{}", uuid::Uuid::new_v4()),
            role: "user".into(),
            parts,
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

        // Slash-skills: the transcript keeps the `/name` the user typed; the
        // harness gets the expanded prompt.
        let mut turn_text = crate::local::skills::expand(&text).unwrap_or(text);
        // Harnesses take plain text; pasted images ride as on-disk paths every
        // CLI can open with its own image-viewing tool.
        if !saved_images.is_empty() {
            let list: String = saved_images
                .iter()
                .map(|(_, path)| format!("- {}\n", path.display()))
                .collect();
            turn_text.push_str(&format!(
                "\n\n<attached-images>\nThe user attached {} image(s) to this message, saved at:\n{list}\
                 View them with your image-reading tool (Read / view_image) before responding.\n</attached-images>",
                saved_images.len()
            ));
        }

        let sid = session.id.clone();
        let mut ctx = TurnCtx {
            host: self.clone(),
            session_id: session.id.clone(),
            harness: session.harness.clone(),
            native_session_id: session.native_session_id.clone(),
            model: session.model.clone(),
            permission_mode: session
                .permission_mode
                .as_deref()
                .and_then(crate::local::harness::PermissionMode::from_id),
            reasoning_level: session.reasoning_level.clone(),
            project,
            text: turn_text,
            assistant: WireMessage {
                id: format!("msg_{}", uuid::Uuid::new_v4()),
                role: "assistant".into(),
                parts: Vec::new(),
                created_at: now_ms(),
            },
            last_flush: Instant::now() - FLUSH_INTERVAL,
        };
        let task = tokio::spawn(async move {
            let result = match crate::local::harness::chat_harness(&ctx.harness) {
                Some(harness) => harness.run_turn(&mut ctx).await,
                None => Err(anyhow!("unknown harness: {}", ctx.harness)),
            };
            if let Err(err) = result {
                ctx.push_error(format!("{err}"));
            }
            let _ = ctx.flush();
            ctx.host.finish_turn(&ctx.session_id).await;
        });
        // Upgrade the reservation None→Some(handle), atomically re-checking that
        // it's still ours: an `interrupt` racing the prologue above may have
        // removed the reservation (and already emitted idle). If so, the freshly
        // spawned task must be aborted — never re-inserted — or it would run to
        // completion uninterruptible while the UI shows the session idle.
        {
            let mut turns = self.turns.lock().await;
            if matches!(turns.get(&sid), Some(None)) {
                turns.insert(sid, Some(task.abort_handle()));
            } else {
                // Reservation gone (interrupted) or already replaced — honor the
                // interrupt: abort the task (its finish_turn won't run) and leave
                // the slot exactly as interrupt left it.
                task.abort();
            }
            // Defuse in BOTH branches while still holding the lock: the guard's
            // Drop must never run after this, or a same-session send that claims
            // a fresh reservation in the gap before Drop could have it clobbered.
            _guard.defuse();
        }
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
        // Outer None: not busy. Inner None: reserved but not yet spawned — the
        // reservation is now cleared, so send_message's guard will abort setup.
        let Some(handle) = self.turns.lock().await.remove(session_id) else {
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
        if let Some(handle) = handle {
            handle.abort();
        }
        self.finish_turn(session_id).await;
        Ok(())
    }

    /// Answer an interactive prompt (plan / permission / question) and resume.
    ///
    /// `ChatHost` owns the harness-agnostic orchestration — locate the
    /// unresolved card, mark it resolved, broadcast — but the *harness* decides
    /// (and, for inline-approval harnesses, performs) how the answer flows back,
    /// via [`Harness::resume_from_prompt`]. That split is deliberate: Claude ends
    /// its turn on a prompt and resumes with a new user message
    /// ([`ResumeAction::SendMessage`]), while OpenCode is still mid-turn, paused
    /// over its serve session, and the reply is POSTed to that live process
    /// ([`ResumeAction::Handled`]) — so a busy session is *expected* there and
    /// must not be rejected.
    pub async fn respond(self: &Arc<Self>, req: PromptAnswer) -> Result<()> {
        // Serialize answers to one session: the load→deliver→resolve sequence
        // below is non-idempotent (an inline reply POSTs to the live harness), so
        // two racing `respond`s (a double-click, two tabs) must not interleave.
        // The loser waits, then finds the card already resolved and no-ops. Held
        // for the whole critical section.
        let gate = self.respond_lock(&req.session_id).await;
        let _gate = gate.lock().await;

        // Load the session and the *unresolved* prompt card (full WirePrompt, so
        // the harness can read its reply target — e.g. opencode's permission id).
        // Nothing is mutated yet, so any error below leaves the card actionable.
        // A card already resolved (the loser of the race above, or a re-submit)
        // is a clean no-op — `unresolved_prompt` returns `None`.
        let session = Store::open()?
            .get_chat_session(&req.session_id)?
            .ok_or_else(|| anyhow!("chat session not found"))?;
        // Already resolved (the loser of a double-submit, or a re-click) is a
        // clean no-op — NOT an error. Returning `Err` here would make the UI's
        // catch clear `busy` on a session whose turn is still streaming; a plain
        // `Ok` leaves the live turn (and its busy state) untouched.
        let Some(prompt) = unresolved_prompt(&req.session_id, &req.prompt_id)? else {
            return Ok(());
        };
        let harness = crate::local::harness::chat_harness(&session.harness)
            .ok_or_else(|| anyhow!("unknown harness: {}", session.harness))?;

        // Ask the harness how the answer resumes. Inline harnesses deliver the
        // reply to their live process here and return `Handled`; end-turn
        // harnesses return the follow-up message to send. Answer validation
        // (e.g. a question with no selection) surfaces as an `Err` here, before
        // we mark anything resolved — so a failed delivery leaves the card
        // actionable and retryable (nothing has been mutated yet).
        let resume_ctx = ResumeCtx {
            host: self.clone(),
            session_id: session.id.clone(),
            native_session_id: session.native_session_id.clone(),
        };
        let action = harness
            .resume_from_prompt(&resume_ctx, &prompt, &req)
            .await?;

        // Mark resolved and broadcast the updated card so it renders read-only
        // on every client immediately (send_message only emits the new user
        // message, never the mutated assistant one).
        let resolved_msg = mark_prompt_resolved(&self.msg_write, &req.session_id, &req.prompt_id)?
            .ok_or_else(|| anyhow!("prompt not found"))?;
        self.emit("chat.message", message_json(&resolved_msg, &req.session_id));

        match action {
            ResumeAction::SendMessage { text, mode } => {
                // End-turn resume: the CLI turn already finished, so the session
                // should be idle; `send_message`'s own guard rejects if a resume
                // turn is somehow already running.
                let overrides = TurnOverrides {
                    model: None,
                    permission_mode: mode.map(|m| m.id().to_string()),
                    reasoning_level: None,
                };
                self.send_message(&req.session_id, text, overrides, Vec::new())
                    .await
            }
            ResumeAction::Handled => {
                // The inline reply unblocked the still-running turn; it keeps
                // streaming and will `finish_turn` itself. Leave `busy` alone.
                Ok(())
            }
            ResumeAction::Nothing => {
                // Card closed with no resume (e.g. a denied Claude permission);
                // broadcast idle so `busy` clears in the UI.
                if let Ok(Some(session)) = Store::open()?.get_chat_session(&req.session_id) {
                    self.emit(
                        "chat.session",
                        json!({ "session": session_json(&session, false) }),
                    );
                }
                Ok(())
            }
        }
    }

    pub async fn delete_session(&self, session_id: &str) -> Result<()> {
        let _ = self.interrupt(session_id).await;
        // Drop the session's respond lock so the map doesn't retain an entry for
        // a session that no longer exists.
        self.respond_locks.lock().await.remove(session_id);
        Store::open()?.delete_chat_session(session_id)?;
        Ok(())
    }
}

/// A user's answer to an interactive prompt.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptAnswer {
    pub session_id: String,
    pub prompt_id: String,
    /// Approve (proceed) vs reject (dismiss). For questions, always true.
    #[serde(default = "default_true")]
    pub approve: bool,
    /// For plan/permission approval: the permission mode to resume under
    /// (a harness-agnostic wire id, e.g. `"auto"`, `"accept-edits"`). None keeps
    /// the session's mode. Only meaningful for end-turn resume (Claude); inline
    /// harnesses reply over their live protocol and ignore it.
    #[serde(default)]
    pub resume_mode: Option<String>,
    /// For questions: the chosen option labels.
    #[serde(default)]
    pub answers: Vec<String>,
    /// Optional freeform note the user added (plan refinement / extra context).
    #[serde(default)]
    pub note: Option<String>,
}

fn default_true() -> bool {
    true
}

/// What a harness needs to resume an answered prompt over its own machinery —
/// handed to [`Harness::resume_from_prompt`]. End-turn harnesses ignore it (they
/// just build a `SendMessage`); inline harnesses reach through `host` to talk to
/// their live process. Kept harness-neutral: it carries the shared `host`, the
/// orx session id, and the native session id, and each harness pulls what it
/// needs (an opencode reply reaches `host.opencode` / `host.http`, exactly as
/// `interrupt` does).
pub struct ResumeCtx {
    pub host: Arc<ChatHost>,
    /// The orx session id (for the `is_busy` liveness check).
    pub session_id: String,
    /// The harness's own session id, if one has been minted (opencode needs it
    /// to address the reply endpoint).
    pub native_session_id: Option<String>,
}

impl ResumeCtx {
    /// Shared HTTP client (mirrors `TurnCtx::http`).
    pub fn http(&self) -> &reqwest::Client {
        &self.host.http
    }

    /// Whether the session still has a turn in flight. An inline harness whose
    /// turn has already ended (errored / been interrupted) has no paused process
    /// left to receive a reply, so it uses this to reject a stale answer instead
    /// of firing a reply into the void.
    pub async fn is_busy(&self) -> bool {
        self.host.is_busy(&self.session_id).await
    }
}

/// The still-*unresolved* prompt card with `prompt_id`, if present — read before
/// any mutation so the harness can inspect it (kind, reply target) and validate
/// the answer first. Returns `None` if there's no such card *or* it's already
/// resolved, so a double-answer is a no-op rather than a second resume.
fn unresolved_prompt(session_id: &str, prompt_id: &str) -> Result<Option<WirePrompt>> {
    let store = Store::open()?;
    for msg in store.list_chat_messages(session_id)?.iter().rev() {
        if msg.role != "assistant" {
            continue;
        }
        let parts: Vec<WirePart> = serde_json::from_str(&msg.parts_json).unwrap_or_default();
        if let Some(prompt) = parts
            .iter()
            .find(|p| p.id == prompt_id)
            .and_then(|p| p.prompt.as_ref())
        {
            return Ok((!prompt.resolved).then(|| prompt.clone()));
        }
    }
    Ok(None)
}

/// Flip the `resolved` flag on the prompt part with `prompt_id` in the session's
/// last assistant message that carries it, persist it, and return the mutated
/// message (so the caller can broadcast a `chat.message` and the card renders
/// read-only). `None` if no such prompt part exists.
///
/// The read→modify→write runs under `msg_write` so it's atomic against a
/// still-running turn's `flush` reconcile-and-persist of the same message (see
/// `TurnCtx::flush`) — otherwise the flush could clobber this resolve.
fn mark_prompt_resolved(
    msg_write: &std::sync::Mutex<()>,
    session_id: &str,
    prompt_id: &str,
) -> Result<Option<WireMessage>> {
    let _guard = msg_write.lock().unwrap();
    let store = Store::open()?;
    for msg in store.list_chat_messages(session_id)?.iter().rev() {
        if msg.role != "assistant" {
            continue;
        }
        let mut parts: Vec<WirePart> = serde_json::from_str(&msg.parts_json).unwrap_or_default();
        if let Some(part) = parts
            .iter_mut()
            .find(|p| p.id == prompt_id && p.prompt.is_some())
        {
            if let Some(prompt) = part.prompt.as_mut() {
                prompt.resolved = true;
            }
            store.upsert_chat_message(&StoredChatMessage {
                id: msg.id.clone(),
                session_id: session_id.to_string(),
                role: msg.role.clone(),
                parts_json: serde_json::to_string(&parts)?,
                created_at: msg.created_at,
            })?;
            return Ok(Some(WireMessage {
                id: msg.id.clone(),
                role: msg.role.clone(),
                parts,
                created_at: msg.created_at,
            }));
        }
    }
    Ok(None)
}

// --- per-turn context handed to adapters --------------------------------------

/// Composer selections a single message can override, mirroring the sticky
/// per-session settings. Empty/None fields leave the stored value in place.
#[derive(Debug, Default)]
pub struct TurnOverrides {
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    pub reasoning_level: Option<String>,
}

pub struct TurnCtx {
    pub host: Arc<ChatHost>,
    pub session_id: String,
    pub harness: String,
    pub native_session_id: Option<String>,
    pub model: Option<String>,
    /// Effective permission mode for this turn (session value; harness applies
    /// its own default when `None`).
    pub permission_mode: Option<crate::local::harness::PermissionMode>,
    /// Effective reasoning-level wire id for this turn (harness-owned vocabulary;
    /// the harness interprets it, e.g. Claude → `--effort`). Default when `None`.
    pub reasoning_level: Option<String>,
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
            prompt: None,
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
        // A prompt card the harness surfaced mid-turn (opencode's inline
        // permission/question) may be answered *while the turn is still running*
        // — `respond` flips its `resolved` flag on the persisted message from a
        // different task. This in-memory copy still has it `false`, so a naive
        // rewrite would revert the card to actionable. Carry forward any
        // already-resolved flag from the store, then persist — under `msg_write`
        // so the read+write is atomic against a concurrent `mark_prompt_resolved`
        // (else that reconcile-then-clobber is a lost update). Only pay the lock
        // when this message actually carries a prompt part.
        let has_prompt = self.assistant.parts.iter().any(|p| p.prompt.is_some());
        {
            // Clone the host handle so the guard borrows it, not `self` — the
            // reconcile below needs `&mut self`.
            let host = self.host.clone();
            let _guard = has_prompt.then(|| host.msg_write.lock().unwrap());
            if has_prompt {
                self.adopt_resolved_prompts(&store);
            }
            store.upsert_chat_message(&StoredChatMessage {
                id: self.assistant.id.clone(),
                session_id: self.session_id.clone(),
                role: "assistant".into(),
                parts_json: serde_json::to_string(&self.assistant.parts)?,
                created_at: self.assistant.created_at,
            })?;
        }
        self.host.emit(
            "chat.message",
            message_json(&self.assistant, &self.session_id),
        );
        Ok(())
    }

    /// Merge the persisted `resolved` state of prompt parts into the in-memory
    /// assistant message, so a concurrent `respond` that resolved a card isn't
    /// clobbered by this turn's next flush. Only ever flips `false`→`true`
    /// (a card never un-resolves), so it's safe regardless of ordering.
    fn adopt_resolved_prompts(&mut self, store: &Store) {
        let Ok(Some(stored)) = store.get_chat_message(&self.assistant.id) else {
            return;
        };
        let persisted: Vec<WirePart> = serde_json::from_str(&stored.parts_json).unwrap_or_default();
        for part in self.assistant.parts.iter_mut() {
            let Some(prompt) = part.prompt.as_mut() else {
                continue;
            };
            if prompt.resolved {
                continue;
            }
            let resolved_in_store = persisted
                .iter()
                .find(|p| p.id == part.id)
                .and_then(|p| p.prompt.as_ref())
                .is_some_and(|p| p.resolved);
            if resolved_in_store {
                prompt.resolved = true;
            }
        }
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
        let Ok(runs) = store.list_runs(200) else {
            continue;
        };
        for run in runs {
            let prev = seen.insert(run.id.clone(), run.status.clone());
            let newly_terminal =
                is_terminal(&run.status) && !matches!(prev.as_deref(), Some(s) if is_terminal(s));
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
            if let Err(err) = chat
                .send_message(&session.id, text, TurnOverrides::default(), Vec::new())
                .await
            {
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
