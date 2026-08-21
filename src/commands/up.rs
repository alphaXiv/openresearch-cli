//! `orx up` — the local autoresearch dashboard server.
//!
//! One axum process on 127.0.0.1 serving three surfaces:
//!   /            embedded SPA (rust-embed over ui/dist, index.html fallback)
//!   /api/*       JSON over the local SQLite store + run-log files
//!   /api/events  SSE: 500ms store + log-file diff loop (serve.rs idiom)
//!   /opencode/*  streaming reverse proxy to the locally spawned `opencode serve`
//!
//! Fully local: no OpenResearch api anywhere on these paths (the /api/papers
//! routes proxy alphaXiv's public, token-free endpoints — needed because the
//! browser can't call api.alphaxiv.org cross-origin). No auth — the bind is
//! loopback-only.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, Method, StatusCode, Uri};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use base64::Engine as _;
use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::error::{anyhow, Result};
use crate::local;
use crate::local::chat::ChatHost;
use crate::local::opencode::AgentHost;
use crate::store::{
    log_path, now_ms, SshHostTest, Store, StoredAgentSelection, StoredChatSession, StoredRun,
};
use crate::updates;
use crate::{browser, UpArgs};

pub async fn run(args: UpArgs) -> Result<()> {
    let port = args.port;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| anyhow!("Could not bind 127.0.0.1:{}: {}", port, e))?;
    // Open early so the schema exists before any request or agent spawn.
    {
        let store = Store::open()?;
        local::chat::reconcile_unfinished_turns(&store)?;
        for run in store.list_active_runs()? {
            if store.get_local_experiment(&run.experiment_id)?.is_some() {
                if let Err(err) = crate::commands::exp::spawn_detached_supervise(&run.id) {
                    eprintln!("could not recover supervisor for run {}: {err}", run.id);
                }
            }
        }
    }

    // Harnesses spawn lazily on the first message to one of their sessions;
    // no eager agent bring-up. (--no-agent is now a no-op kept for compat.)
    let agent = Arc::new(AgentHost::new(args.model.clone()));
    let codex = Arc::new(local::codex::CodexHost::new());
    let claude = Arc::new(local::claude::ClaudeHost::new());
    claude.start_reaper();
    let state = AppState {
        agent: agent.clone(),
        chat: Arc::new(ChatHost::new(agent.clone(), codex.clone(), claude.clone())),
        claude: claude.clone(),
        harnesses: Arc::new(tokio::sync::Mutex::new(None)),
        project_lifecycle: Arc::new(ProjectLifecycle::default()),
        project_creation_lock: Arc::new(tokio::sync::Mutex::new(())),
        publication_locks: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        data_dir_move_in_progress: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        data_dir_gate: Arc::new(tokio::sync::Mutex::new(())),
    };
    // Plan-mode turns hand this port to the `orx mcp-gate` permission bridge.
    state.chat.set_up_port(port);
    state.chat.resume_persisted_queues();
    {
        let chat = state.chat.clone();
        let moving = state.data_dir_move_in_progress.clone();
        let gate = state.data_dir_gate.clone();
        tokio::spawn(async move {
            let interval =
                Duration::from_millis((crate::store::CHAT_TURN_LEASE_TTL_MS + 1_000) as u64);
            loop {
                tokio::time::sleep(interval).await;
                if moving.load(std::sync::atomic::Ordering::SeqCst) {
                    continue;
                }
                let _gate = gate.lock().await;
                if moving.load(std::sync::atomic::Ordering::SeqCst) {
                    continue;
                }
                if let Err(err) = chat.reconcile_expired_turn_leases() {
                    eprintln!("orx up: could not reconcile expired chat turns: {err}");
                }
            }
        });
    }

    spawn_agent_preflight();
    // Deliver explicitly registered run wake-ups once their chat becomes idle.
    tokio::spawn(local::chat::watch_runs(
        state.chat.clone(),
        state.data_dir_move_in_progress.clone(),
        state.data_dir_gate.clone(),
    ));
    spawn_claude_auth_monitor(state.chat.clone(), claude.clone(), state.harnesses.clone());
    spawn_update_checker();

    let app = router(state);
    let url = format!("http://127.0.0.1:{port}");
    // In an SSH session the loopback URL only works on the remote box and there's
    // no local browser to open — print forwarding guidance instead of the bare
    // URL, and skip the (futile) browser-open. Otherwise, today's local flow.
    if let Some(session) = crate::remote::detect_ssh_session() {
        eprint!("{}", session.instructions(port));
    } else {
        eprintln!("orx up: dashboard on {url}");
        if !args.no_browser {
            browser::open_browser(&url);
        }
    }

    // select! instead of graceful shutdown: open SSE streams never complete,
    // so waiting on connections would hang Ctrl-C forever.
    //
    // We wait on SIGTERM/SIGHUP as well as SIGINT: when this server is started
    // over SSH by `orx up --remote`, closing that tunnel (the launcher's Ctrl-C)
    // delivers SIGHUP here as the channel tears down — without handling it the
    // remote server would leak, staying bound to its port after the tunnel dies.
    tokio::select! {
        r = axum::serve(listener, app) => r.map_err(|e| anyhow!("orx up: server error: {e}"))?,
        _ = shutdown_signal() => eprintln!("orx up: shutting down"),
    }
    agent.shutdown().await;
    codex.shutdown().await;
    claude.shutdown().await;
    Ok(())
}

/// Resolves when the process is asked to stop. SIGINT everywhere; on Unix also
/// SIGTERM and SIGHUP (SIGHUP is what an SSH tunnel delivers on disconnect, so
/// a `--remote`-launched server exits with its tunnel instead of leaking).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, Signal, SignalKind};
        // If a signal stream can't be installed, that arm simply never fires —
        // fall back to whatever handlers do register rather than aborting.
        async fn wait(s: &mut Option<Signal>) {
            match s {
                Some(s) => {
                    s.recv().await;
                }
                None => std::future::pending().await,
            }
        }
        let mut term = signal(SignalKind::terminate()).ok();
        let mut hup = signal(SignalKind::hangup()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = wait(&mut term) => {}
            _ = wait(&mut hup) => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[derive(Clone)]
struct AppState {
    agent: Arc<AgentHost>,
    chat: Arc<ChatHost>,
    claude: Arc<local::claude::ClaudeHost>,
    /// Harness detection cache — detection shells out to CLIs, so it's rate-
    /// limited to once per TTL unless the UI asks for a refresh.
    harnesses: Arc<tokio::sync::Mutex<Option<(std::time::Instant, Value)>>>,
    project_lifecycle: Arc<ProjectLifecycle>,
    project_creation_lock: Arc<tokio::sync::Mutex<()>>,
    publication_locks: Arc<tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// Set while a data-dir move is running. New chat turns and run launches
    /// check it and refuse (409) so nothing starts writing the store mid-move —
    /// closing the window between the move's in-flight check and its completion.
    data_dir_move_in_progress: Arc<std::sync::atomic::AtomicBool>,
    /// Serializes wake-up store writes with a live data-directory move.
    data_dir_gate: Arc<tokio::sync::Mutex<()>>,
}

async fn project_publication_lock(
    state: &AppState,
    project_id: &str,
) -> tokio::sync::OwnedMutexGuard<()> {
    let lock = {
        let mut locks = state.publication_locks.lock().await;
        locks
            .entry(project_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    lock.lock_owned().await
}

#[derive(Default)]
struct ProjectLifecycle {
    inner: Arc<std::sync::Mutex<ProjectLifecycleState>>,
}

#[derive(Default)]
struct ProjectLifecycleState {
    deleting: HashSet<String>,
    admissions: HashMap<String, usize>,
}

struct ProjectAdmissionLease {
    inner: Arc<std::sync::Mutex<ProjectLifecycleState>>,
    project_id: String,
}

impl Drop for ProjectAdmissionLease {
    fn drop(&mut self) {
        let mut state = self.inner.lock().unwrap();
        if let Some(count) = state.admissions.get_mut(&self.project_id) {
            *count -= 1;
            if *count == 0 {
                state.admissions.remove(&self.project_id);
            }
        }
    }
}

struct ProjectDeletionLease {
    inner: Arc<std::sync::Mutex<ProjectLifecycleState>>,
    project_id: String,
}

impl Drop for ProjectDeletionLease {
    fn drop(&mut self) {
        self.inner.lock().unwrap().deleting.remove(&self.project_id);
    }
}

impl ProjectLifecycle {
    fn admit(&self, project_id: &str) -> Option<ProjectAdmissionLease> {
        let mut state = self.inner.lock().unwrap();
        if state.deleting.contains(project_id) {
            return None;
        }
        *state.admissions.entry(project_id.to_string()).or_default() += 1;
        Some(ProjectAdmissionLease {
            inner: self.inner.clone(),
            project_id: project_id.to_string(),
        })
    }

    fn begin_delete(&self, project_id: &str) -> Option<ProjectDeletionLease> {
        let mut state = self.inner.lock().unwrap();
        if state.deleting.contains(project_id)
            || state
                .admissions
                .get("__project_create__")
                .copied()
                .unwrap_or_default()
                > 0
            || state
                .admissions
                .get(project_id)
                .copied()
                .unwrap_or_default()
                > 0
        {
            return None;
        }
        state.deleting.insert(project_id.to_string());
        Some(ProjectDeletionLease {
            inner: self.inner.clone(),
            project_id: project_id.to_string(),
        })
    }

    fn operation_count(&self) -> usize {
        let state = self.inner.lock().unwrap();
        state.admissions.values().copied().sum::<usize>() + state.deleting.len()
    }
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/onboarding/complete", post(complete_onboarding))
        .route("/api/project-path/status", get(project_path_status))
        .route("/api/project-path/pick", post(pick_project_folder))
        .route("/api/projects", get(list_projects).post(create_project))
        .route(
            "/api/projects/{id}",
            get(get_project)
                .patch(update_project)
                .delete(delete_project),
        )
        .route("/api/projects/{id}/open", post(open_project))
        .route("/api/projects/{id}/git", get(project_git_status))
        .route("/api/projects/{id}/git/init", post(initialize_project_git))
        .route("/api/projects/{id}/github", post(enable_project_github))
        .route(
            "/api/projects/{id}/github/disable",
            post(disable_project_github),
        )
        .route("/api/projects/{id}/github/push", post(push_project_github))
        .route("/api/github/account", get(github_account))
        .route(
            "/api/github/project-repo-preview",
            get(github_project_repo_preview),
        )
        .route("/api/github/repo-access", get(github_repo_access))
        .route("/api/projects/{id}/experiments", get(list_experiments))
        .route("/api/projects/{id}/runs", get(list_project_runs))
        .route("/api/papers/search", get(search_papers_api))
        .route("/api/papers/resolve", get(resolve_paper_api))
        .route("/api/compute/backends", get(compute_backends))
        .route("/api/runs", post(create_run))
        .route("/api/runs/{id}", get(get_run))
        .route("/api/instances", get(list_instances))
        .route("/api/runs/{id}/cancel", post(cancel_run))
        .route("/api/runs/{id}/log", get(run_log))
        .route("/api/runs/{id}/logs", get(run_logs))
        .route("/api/runs/{id}/diff", get(run_diff))
        .route("/api/experiments/{id}/diff", get(experiment_diff))
        .route("/api/experiments/{id}/commits", get(experiment_commits))
        .route(
            "/api/experiments/{id}/commits/{sha}/diff",
            get(experiment_commit_diff),
        )
        .route("/api/projects/{id}/working-tree", get(project_working_tree))
        .route("/api/projects/{id}/code-tree", get(project_code_tree))
        .route(
            "/api/projects/{id}/file",
            get(project_file).put(write_project_file),
        )
        .route("/api/projects/{id}/file/raw", get(project_raw_file))
        .route("/api/projects/{id}/file/open", post(open_project_file))
        .route("/api/files/abs", get(absolute_file))
        .route("/api/files/abs/raw", get(absolute_raw_file))
        .route(
            "/api/projects/{id}/files",
            get(list_artifacts).delete(delete_artifact),
        )
        .route("/api/projects/{id}/files/file", get(serve_artifact))
        .route(
            "/api/projects/{id}/brief",
            put(write_project_brief).layer(DefaultBodyLimit::max(
                local::files::MAX_PROJECT_BRIEF_BYTES * 6 + 1024,
            )),
        )
        .route("/api/events", get(events))
        .route("/api/settings/hf", get(hf_settings).post(set_hf_token))
        .route(
            "/api/settings/k8s",
            get(k8s_settings).post(set_k8s_settings),
        )
        .route("/api/settings/modal", get(modal_settings))
        .route("/api/settings/modal/provision", post(provision_modal))
        .route("/api/settings/env", get(env_settings).post(set_env_var))
        .route(
            "/api/settings/env/{key}",
            axum::routing::delete(delete_env_var),
        )
        .route(
            "/api/settings/data-dir",
            get(data_dir_settings).post(set_data_dir),
        )
        .route("/api/settings/data-dir/validate", post(validate_data_dir))
        .route("/api/settings/data-dir/move", post(move_data_dir))
        .route(
            "/api/settings/git",
            get(git_settings).post(set_git_settings),
        )
        .route(
            "/api/settings/git/token",
            post(set_git_token).delete(delete_git_token),
        )
        .route(
            "/api/settings/projects",
            get(project_defaults).post(set_project_defaults),
        )
        .route(
            "/api/settings/telemetry",
            get(telemetry_settings).post(set_telemetry_settings),
        )
        .route(
            "/api/settings/telemetry/consent",
            post(record_telemetry_consent),
        )
        .route(
            "/api/settings/profile",
            get(profile_settings).post(set_profile_settings),
        )
        .route("/api/update", get(update_status))
        .route("/api/update/apply", post(apply_update))
        .route("/api/update/auto", post(set_auto_update))
        .route("/api/update/install-cli", post(install_cli))
        .route("/api/settings/ui-state", get(ui_state).post(set_ui_state))
        .route("/api/settings/ssh", get(ssh_settings))
        .route("/api/settings/ssh/preflight", post(ssh_preflight))
        .route(
            "/api/settings/slurm",
            get(slurm_settings).post(set_slurm_settings),
        )
        .route("/api/settings/slurm/preflight", post(slurm_preflight))
        .route(
            "/api/settings/ray",
            get(ray_settings).post(set_ray_settings),
        )
        .route("/api/settings/ray/preflight", post(ray_preflight))
        .route("/api/settings/compute", get(compute_settings))
        .route("/api/settings/compute/default", post(set_compute_default))
        .route("/api/settings/local", get(local_machine_settings))
        .route("/api/settings/openresearch", get(openresearch_settings))
        .route(
            "/api/settings/lit-sources",
            get(lit_sources_settings).post(set_lit_sources_settings),
        )
        .route("/api/harnesses", get(list_harnesses))
        .route("/api/skills", get(list_skills))
        .route(
            "/api/user-skills",
            get(list_user_skills)
                .post(upload_user_skill)
                .delete(delete_user_skill),
        )
        .route("/api/user-skills/import", post(import_user_skill))
        .route("/api/harness-skills", get(list_harness_skills))
        .route(
            "/api/chat/sessions",
            get(list_chat_sessions).post(create_chat_session),
        )
        .route(
            "/api/chat/sessions/{id}",
            axum::routing::delete(delete_chat_session).patch(update_chat_session),
        )
        .route("/api/chat/sessions/{id}/messages", get(chat_messages))
        .route("/api/chat/sessions/{id}/worktree", get(session_worktree))
        .route("/api/chat/sessions/{id}/message", post(send_chat_message))
        .route(
            "/api/chat/sessions/{id}/turns/{turnId}/recover",
            post(recover_chat_turn),
        )
        .route("/api/chat/sessions/{id}/fork", post(fork_chat_turn))
        .route("/api/chat/sessions/{id}/branch", post(select_chat_branch))
        .route("/api/chat/sessions/{id}/interrupt", post(interrupt_chat))
        .route(
            "/api/chat/sessions/{id}/queue/{itemId}",
            axum::routing::delete(cancel_queued_chat).post(retry_queued_chat),
        )
        .route("/api/chat/sessions/{id}/respond", post(respond_chat))
        // Internal: the `orx mcp-gate` permission bridge's long-poll (plan
        // mode). Token-authenticated in the handler; blocks until the surfaced
        // card is answered.
        .route("/api/internal/permissions", post(bridge_permission))
        .route("/api/chat/attachments/{name}", get(chat_attachment))
        .route("/api/agent/status", get(agent_status))
        .fallback(spa)
        // Chat attachments (PDFs, images) ride as base64 in the send-message
        // JSON body; the 2 MB axum default rejects any real paper. Cap it well
        // above the client-side per-file limit so a full message still fits.
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .with_state(state)
}

// --- error plumbing -------------------------------------------------------

/// JSON error responses: `{"error": "..."}` with an explicit status.
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<crate::error::Error> for ApiError {
    fn from(err: crate::error::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
    }
}

fn bad_request(err: impl std::fmt::Display) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, err.to_string())
}

fn not_found(what: &str) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, format!("{what} not found"))
}

type ApiResult = std::result::Result<Json<Value>, ApiError>;

// --- wire types -----------------------------------------------------------

/// The Run entity the API serves: StoredRun with `backend_json` parsed into an
/// object and cancellation intent exposed for pending UI state.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ApiRun {
    id: String,
    experiment_id: String,
    project_id: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    backend: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result_markdown: Option<String>,
    created_at: i64,
    updated_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    ended_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i64>,
    cancel_requested: bool,
}

impl From<&StoredRun> for ApiRun {
    fn from(run: &StoredRun) -> Self {
        Self {
            id: run.id.clone(),
            experiment_id: run.experiment_id.clone(),
            project_id: run.project_id.clone(),
            status: run.status.clone(),
            backend: serde_json::from_str(&run.backend_json).ok(),
            command: Some(run.command.clone()).filter(|c| !c.is_empty()),
            commit_sha: run.commit_sha.clone(),
            result_markdown: run.result_markdown.clone(),
            created_at: run.created_at,
            updated_at: run.updated_at,
            ended_at: run.ended_at,
            exit_code: run.exit_code,
            cancel_requested: run.cancel_requested,
        }
    }
}

// --- basic routes ---------------------------------------------------------

async fn health() -> Json<Value> {
    Json(json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompleteOnboardingReq {
    harness: String,
    model: Option<String>,
    permission_mode: Option<String>,
    reasoning_level: Option<String>,
    #[serde(default)]
    research_areas: Vec<String>,
    #[serde(default)]
    other_area: Option<String>,
    #[serde(default)]
    background: Option<String>,
    #[serde(default)]
    papers: Vec<crate::telemetry::ProfilePaper>,
}

const RESEARCH_AREAS: [&str; 4] = ["AI/ML", "Biology", "Physics", "Other"];

fn preferred_permission_mode(harness: &str, mode: Option<String>) -> Option<String> {
    match mode.as_deref() {
        Some("plan") => local::harness::effective_permission_id(harness, None),
        _ => mode,
    }
}

fn normalize_research_profile(
    research_areas: Vec<String>,
    other_area: Option<String>,
    background: Option<String>,
    papers: Vec<crate::telemetry::ProfilePaper>,
) -> std::result::Result<crate::telemetry::ResearchProfile, ApiError> {
    if research_areas.is_empty() {
        return Err(bad_request("choose at least one research area"));
    }
    let mut normalized_areas = Vec::with_capacity(research_areas.len());
    for area in research_areas {
        let area = area.trim().to_string();
        if !RESEARCH_AREAS.contains(&area.as_str()) {
            return Err(bad_request(format!("unknown research area: {area}")));
        }
        if normalized_areas.contains(&area) {
            return Err(bad_request(format!("duplicate research area: {area}")));
        }
        normalized_areas.push(area);
    }
    let other_area = other_area.filter(|value| !value.trim().is_empty());
    let includes_other = normalized_areas.iter().any(|area| area == "Other");
    if includes_other && other_area.is_none() {
        return Err(bad_request(
            "describe your research area when choosing Other",
        ));
    }
    if !includes_other && other_area.is_some() {
        return Err(bad_request(
            "choose Other before describing another research area",
        ));
    }
    Ok(crate::telemetry::ResearchProfile {
        research_areas: normalized_areas,
        other_area,
        background: background.filter(|value| !value.trim().is_empty()),
        papers,
    })
}

async fn complete_onboarding(
    State(state): State<AppState>,
    Json(req): Json<CompleteOnboardingReq>,
) -> ApiResult {
    reject_if_moving(&state)?;
    if !local::harness::is_chat_harness(&req.harness) {
        return Err(bad_request(format!("unknown harness: {}", req.harness)));
    }
    let nonempty = |value: Option<String>| value.filter(|item| !item.trim().is_empty());
    let permission_mode = nonempty(req.permission_mode);
    if permission_mode
        .as_deref()
        .is_some_and(|mode| local::harness::permission_mode_for(&req.harness, mode).is_none())
    {
        return Err(bad_request("invalid permission mode for selected harness"));
    }
    let selection = local::demo::DemoSelection {
        harness: req.harness,
        model: nonempty(req.model),
        permission_mode,
        reasoning_level: nonempty(req.reasoning_level),
    };
    let profile = normalize_research_profile(
        req.research_areas,
        req.other_area,
        req.background,
        req.papers,
    )?;
    let profile_for_event = profile.clone();
    let completion = tokio::task::spawn_blocking(move || -> Result<_> {
        let _ = crate::telemetry::set_profile(profile);
        let completion = local::demo::complete_onboarding(selection)?;
        let store = Store::open()?;
        store.set_preferred_agent(&StoredAgentSelection {
            harness: completion.selection.harness.clone(),
            model: completion.selection.model.clone(),
            permission_mode: preferred_permission_mode(
                &completion.selection.harness,
                completion.selection.permission_mode.clone(),
            ),
            reasoning_level: completion.selection.reasoning_level.clone(),
        })?;
        store.set_onboarding_completed(true)?;
        Ok(completion)
    })
    .await
    .map_err(|e| ApiError::from(anyhow!("demo seed task failed: {e}")))??;
    if completion.newly_created {
        crate::telemetry::capture_onboarding_completed();
        crate::telemetry::capture_onboarding_research_profile(&profile_for_event);
    }
    Ok(Json(json!({
        "project": project_json(&completion.project),
        "selection": completion.selection,
    })))
}

#[derive(Deserialize)]
struct SkillsQ {
    /// Include the project's own uploaded skills (plus globals) in the menu.
    project: Option<String>,
}

/// Slash-skills the composer's `/` dropdown offers (expanded server-side): the
/// built-in catalog plus any user-uploaded skills that apply (globals, and the
/// named project's own).
async fn list_skills(Query(q): Query<SkillsQ>) -> Json<Value> {
    let mut skills: Vec<Value> = crate::local::skills::CATALOG
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "description": s.description,
                "argHint": s.arg_hint,
                "source": "builtin",
            })
        })
        .collect();
    // One `/name` per skill: a project skill shadows a same-named global
    // (list_for_project returns globals first, then the project's own).
    let mut user: Vec<crate::local::user_skills::UserSkill> = Vec::new();
    for s in crate::local::user_skills::list_for_project(q.project.as_deref()) {
        match user.iter_mut().find(|e| e.name == s.name) {
            Some(existing) => *existing = s,
            None => user.push(s),
        }
    }
    for s in user {
        skills.push(json!({
            "name": s.name,
            "description": s.description,
            "argHint": "",
            "source": "user",
        }));
    }
    Json(json!({ "skills": skills }))
}

// --- user-uploaded skills -----------------------------------------------------

fn parse_scope(scope: &str) -> std::result::Result<crate::local::user_skills::Scope, ApiError> {
    match scope {
        "global" => Ok(crate::local::user_skills::Scope::Global),
        "project" => Ok(crate::local::user_skills::Scope::Project),
        other => Err(bad_request(format!("unknown scope `{other}`"))),
    }
}

fn user_skill_json(s: &crate::local::user_skills::UserSkill) -> Value {
    json!({
        "name": s.name,
        "description": s.description,
        "scope": s.scope,
        "bytes": s.bytes,
        "updatedAt": s.updated_at,
    })
}

/// Resolve the target scope and validate the project exists for project scope.
fn resolve_skill_scope(
    scope: crate::local::user_skills::Scope,
    project_id: Option<&str>,
) -> std::result::Result<Option<String>, ApiError> {
    match scope {
        crate::local::user_skills::Scope::Global => Ok(None),
        crate::local::user_skills::Scope::Project => {
            let id = project_id
                .filter(|s| !s.is_empty())
                .ok_or_else(|| bad_request("project scope requires a projectId"))?;
            Store::open()?
                .get_local_project(id)?
                .ok_or_else(|| not_found("project"))?;
            Ok(Some(id.to_string()))
        }
    }
}

#[derive(Deserialize)]
struct UserSkillsListQ {
    project: Option<String>,
}

/// Both scopes for the Skills tab: globals plus the project's own.
async fn list_user_skills(Query(q): Query<UserSkillsListQ>) -> ApiResult {
    let skills: Vec<Value> = crate::local::user_skills::list_for_project(q.project.as_deref())
        .iter()
        .map(user_skill_json)
        .collect();
    Ok(Json(json!({ "skills": skills })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadSkillReq {
    scope: String,
    project_id: Option<String>,
    /// Original upload filename — its extension picks `.zip` vs single file.
    filename: String,
    /// The file bytes, base64 (same convention as chat attachments).
    content_base64: String,
}

async fn upload_user_skill(Json(req): Json<UploadSkillReq>) -> ApiResult {
    let scope = parse_scope(&req.scope)?;
    let project = resolve_skill_scope(scope, req.project_id.as_deref())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(req.content_base64.trim())
        .map_err(|e| bad_request(format!("invalid file data: {e}")))?;

    let lower = req.filename.to_ascii_lowercase();
    let saved = if lower.ends_with(".zip") {
        crate::local::user_skills::save_zip(&bytes, scope, project.as_deref())
    } else if lower.ends_with(".md") || lower.ends_with(".markdown") {
        crate::local::user_skills::save_skill_md(&bytes, scope, project.as_deref())
    } else {
        return Err(bad_request(
            "upload a SKILL.md file or a .zip of a skill folder",
        ));
    }
    .map_err(bad_request)?;

    Ok(Json(json!({ "skill": user_skill_json(&saved) })))
}

#[derive(Deserialize)]
struct DeleteSkillQ {
    scope: String,
    name: String,
    project: Option<String>,
}

async fn delete_user_skill(Query(q): Query<DeleteSkillQ>) -> ApiResult {
    let scope = parse_scope(&q.scope)?;
    let project = resolve_skill_scope(scope, q.project.as_deref())?;
    crate::local::user_skills::delete(&q.name, scope, project.as_deref()).map_err(bad_request)?;
    Ok(Json(json!({ "ok": true })))
}

/// Skills already installed in the user's coding agents, offered for import.
async fn list_harness_skills() -> ApiResult {
    let skills: Vec<Value> = crate::local::user_skills::list_harness_skills()
        .iter()
        .map(|s| {
            json!({
                "harnessId": s.harness_id,
                "harnessName": s.harness_name,
                "name": s.name,
                "description": s.description,
            })
        })
        .collect();
    Ok(Json(json!({ "skills": skills })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImportSkillReq {
    harness: String,
    name: String,
    scope: String,
    project_id: Option<String>,
}

async fn import_user_skill(Json(req): Json<ImportSkillReq>) -> ApiResult {
    let scope = parse_scope(&req.scope)?;
    let project = resolve_skill_scope(scope, req.project_id.as_deref())?;
    let saved = crate::local::user_skills::import_from_harness(
        &req.harness,
        &req.name,
        scope,
        project.as_deref(),
    )
    .map_err(bad_request)?;
    Ok(Json(json!({ "skill": user_skill_json(&saved) })))
}

/// Serialize a project for the UI, injecting the absolute artifacts directory
/// so the dashboard can recognize artifact paths in chat links. `filesDir` is
/// retained as a compatibility alias for older clients. Every project the UI
/// receives must go through this — the SSE `project.updated` diff (fired on
/// every visit, since `open_project` bumps `updated_at`) and `create_project`
/// upsert into the same `projects` state as `list_projects`.
fn project_json(p: &local::model::LocalProject) -> Value {
    project_json_with_artifacts_dir(p, local::files::files_dir_display(p))
}

fn project_json_with_artifacts_dir(p: &local::model::LocalProject, artifacts_dir: String) -> Value {
    // LocalProject is all String/Option/i64, so this can't realistically fail;
    // fail loud rather than emit a malformed `null` project if it ever does.
    let mut v = serde_json::to_value(p).expect("LocalProject serializes");
    if let Value::Object(map) = &mut v {
        let dir = Value::String(artifacts_dir);
        map.insert("artifactsDir".into(), dir.clone());
        map.insert("filesDir".into(), dir);
        map.insert("path".into(), Value::String(p.repo_path.clone()));
        map.insert("githubEnabled".into(), Value::Bool(p.github_enabled()));
        map.insert(
            "githubUrl".into(),
            p.github_url().map(Value::String).unwrap_or(Value::Null),
        );
    }
    v
}

async fn list_projects() -> ApiResult {
    let projects = Store::open()?.list_local_projects()?;
    let projects: Vec<Value> = projects.iter().map(project_json).collect();
    Ok(Json(json!({ "projects": projects })))
}

#[derive(Deserialize)]
struct ProjectPathStatusQ {
    path: Option<String>,
}

async fn project_path_status(Query(q): Query<ProjectPathStatusQ>) -> ApiResult {
    tokio::task::spawn_blocking(move || -> Result<Json<Value>> {
        let git_version = local::git::version();
        let Some(path) = q.path.filter(|path| !path.trim().is_empty()) else {
            return Ok(Json(json!({
                "gitVersion": git_version,
                "resolvedPath": null,
                "exists": null,
                "directory": null,
                "empty": null,
                "initialized": null,
                "gitState": null,
            })));
        };
        let resolved = local::projects::expand_path(&path)?;
        let exists = resolved.exists();
        let directory = resolved.is_dir();
        let empty = if directory {
            Some(std::fs::read_dir(&resolved)?.next().is_none())
        } else {
            None
        };
        let git_state =
            (git_version.is_some() && directory).then(|| local::git::repository_state(&resolved));
        let initialized = git_state.is_some_and(local::git::RepositoryState::is_initialized);
        let github_publication = initialized
            .then(|| local::git::github_publication(&resolved))
            .flatten();
        Ok(Json(json!({
            "gitVersion": git_version,
            "resolvedPath": resolved.to_string_lossy(),
            "exists": exists,
            "directory": directory,
            "empty": empty,
            "initialized": initialized,
            "gitState": git_state.map(local::git::RepositoryState::as_str),
            "githubOwner": github_publication.as_ref().map(|(owner, _)| owner),
            "githubRepo": github_publication.as_ref().map(|(_, repo)| repo),
        })))
    })
    .await
    .map_err(|error| ApiError::from(anyhow!("project path task failed: {error}")))?
    .map_err(bad_request)
}

async fn pick_project_folder() -> ApiResult {
    let path = tokio::task::spawn_blocking(crate::folder_picker::pick_folder)
        .await
        .map_err(|error| ApiError::from(anyhow!("folder picker task failed: {error}")))?
        .map_err(bad_request)?;
    Ok(Json(json!({
        "path": path.map(|path| path.to_string_lossy().into_owned()),
    })))
}

// --- papers (new-project "from a paper" flow; proxies alphaXiv) ------------

#[derive(Deserialize)]
struct PaperSearchQ {
    q: String,
}

async fn search_papers_api(Query(q): Query<PaperSearchQ>) -> ApiResult {
    let query = q.q.trim();
    if query.is_empty() {
        return Ok(Json(json!({ "papers": [] })));
    }
    let papers = crate::client::search_papers_fast(query)
        .await
        .map_err(bad_request)?;
    Ok(Json(json!({ "papers": papers })))
}

#[derive(Deserialize)]
struct PaperResolveQ {
    id: String,
}

async fn resolve_paper_api(Query(q): Query<PaperResolveQ>) -> ApiResult {
    let id = super::paper::parse_paper_id(&q.id);
    if id.is_empty() {
        return Err(bad_request("paper id is required"));
    }
    let paper = crate::client::resolve_paper(&id)
        .await
        .map_err(bad_request)?;
    Ok(Json(json!({ "paper": paper })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateProjectReq {
    name: String,
    path: String,
    run_command: Option<String>,
    paper_id: Option<String>,
    clone_url: Option<String>,
    #[serde(default)]
    create_folder: bool,
    #[serde(default)]
    initialize_git: bool,
    github_sync_enabled: Option<bool>,
}

async fn create_project(
    State(state): State<AppState>,
    Json(req): Json<CreateProjectReq>,
) -> ApiResult {
    reject_if_moving(&state)?;
    let create_admission = state
        .project_lifecycle
        .admit("__project_create__")
        .ok_or_else(|| bad_request("project creation is unavailable"))?;
    reject_if_moving(&state)?;
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return Err(bad_request("name is required"));
    }
    let path = req.path;
    let create_folder = req.create_folder;
    let initialize_git = req.initialize_git;
    let clone_url = req.clone_url.filter(|url| !url.trim().is_empty());
    let paper_id = req.paper_id.filter(|paper_id| !paper_id.trim().is_empty());
    if paper_id.is_some() && clone_url.is_none() {
        return Err(bad_request(
            "A paper project requires a linked public code repository.",
        ));
    }
    let github_sync_enabled = req
        .github_sync_enabled
        .unwrap_or_else(crate::config::github_for_new_projects);
    let repo_size_kb = match clone_url.as_deref() {
        Some(url) => local::github::public_repo_size_kb(url).await,
        None => None,
    };
    let shallow_clone = local::github::should_shallow_clone(repo_size_kb);
    let run_command = req.run_command;
    let creation_guard = state.project_creation_lock.lock().await;
    let result = tokio::task::spawn_blocking(move || -> Result<local::model::LocalProject> {
        let store = Store::open()?;
        let project = local::projects::create_project(
            &store,
            &name,
            &path,
            local::projects::CreateProjectOptions {
                create_folder,
                initialize_git,
                clone_url,
                shallow_clone,
                run_command,
                paper_id,
            },
        )?;
        if let Err(error) = local::files::ensure_project_brief(&project) {
            store.delete_local_project(&project.id)?;
            return Err(error);
        }
        Ok(project)
    })
    .await
    .map_err(|e| anyhow!("project task failed: {e}"))?;
    let project = result.map_err(bad_request)?;
    drop(creation_guard);
    let _project_admission = state
        .project_lifecycle
        .admit(&project.id)
        .ok_or_else(|| bad_request("project deletion is in progress"))?;
    drop(create_admission);
    let (project, github_publication_error) = if github_sync_enabled {
        match push_project_for_sync(project.clone()).await {
            Ok(project) => (project, None),
            Err(error) => {
                let project = Store::open()?
                    .get_local_project(&project.id)?
                    .unwrap_or(project);
                (project, Some(error.to_string()))
            }
        }
    } else {
        (project, None)
    };
    crate::telemetry::capture_project_created(true);
    Ok(Json(json!({
        "project": project_json(&project),
        "githubPublicationError": github_publication_error,
    })))
}

async fn get_project(Path(id): Path<String>) -> ApiResult {
    let project = Store::open()?
        .get_local_project(&id)?
        .ok_or_else(|| not_found("project"))?;
    Ok(Json(json!({ "project": project_json(&project) })))
}

fn github_token_source() -> Option<&'static str> {
    if std::env::var("GITHUB_TOKEN").is_ok_and(|token| !token.trim().is_empty()) {
        Some("env")
    } else if crate::config::synced_env_var("GITHUB_TOKEN").is_some() {
        Some("stored")
    } else {
        local::git::resolve_github_token().map(|_| "gh")
    }
}

fn project_git_json(project: &local::model::LocalProject) -> Value {
    let path = std::path::Path::new(&project.repo_path);
    let initialized = local::git::is_repository(path);
    let branch = initialized
        .then(|| local::git::require_current_branch(path).ok())
        .flatten();
    let clean = initialized
        .then(|| local::git::is_clean(path).ok())
        .flatten();
    let remotes = if initialized {
        local::git::remotes(path)
            .unwrap_or_default()
            .into_iter()
            .map(|(name, url)| json!({ "name": name, "url": url }))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let (name, email, name_source, email_source) = if initialized {
        local::git::identity(path)
    } else {
        (None, None, None, None)
    };
    let sync_status = project.github_enabled().then(|| {
        local::git::publication_sync_status(
            path,
            &project.baseline_branch,
            &project.github_owner,
            &project.github_repo,
        )
    });
    json!({
        "path": project.repo_path,
        "gitVersion": local::git::version(),
        "initialized": initialized,
        "baselineBranch": project.baseline_branch,
        "currentBranch": branch,
        "clean": clean,
        "remotes": remotes,
        "identity": {
            "name": name,
            "email": email,
            "nameSource": name_source,
            "emailSource": email_source,
        },
        "github": {
            "authenticated": github_token_source().is_some(),
            "tokenSource": github_token_source(),
            "enabled": project.github_enabled(),
            "owner": project.github_owner,
            "repo": project.github_repo,
            "url": project.github_url(),
            "syncStatus": sync_status,
        },
    })
}

async fn project_git_status(Path(id): Path<String>) -> ApiResult {
    tokio::task::spawn_blocking(move || {
        let project = Store::open()?
            .get_local_project(&id)?
            .ok_or_else(|| anyhow!("project not found"))?;
        Ok(Json(project_git_json(&project)))
    })
    .await
    .map_err(|error| ApiError::from(anyhow!("git task failed: {error}")))?
}

async fn initialize_project_git(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult {
    reject_if_moving(&state)?;
    let _admission = state
        .project_lifecycle
        .admit(&id)
        .ok_or_else(|| bad_request("project deletion is in progress"))?;
    reject_if_moving(&state)?;
    let _lock = project_publication_lock(&state, &id).await;
    let _creation_guard = state.project_creation_lock.lock().await;
    tokio::task::spawn_blocking(move || {
        let store = Store::open()?;
        let mut project = store
            .get_local_project(&id)?
            .ok_or_else(|| anyhow!("project not found"))?;
        let path = std::path::Path::new(&project.repo_path);
        local::git::initialize_repository(path)?;
        local::git::validate_project_repository(path)?;
        project.baseline_branch = local::git::require_current_branch(path)?;
        store.update_local_project(&project)?;
        Ok(Json(project_git_json(&project)))
    })
    .await
    .map_err(|error| ApiError::from(anyhow!("git task failed: {error}")))?
}

fn push_project(project: &local::model::LocalProject) -> Result<()> {
    if !project.github_enabled() {
        return Err(anyhow!(
            "Enable GitHub syncing for this project before pushing."
        ));
    }
    let path = std::path::Path::new(&project.repo_path);
    local::git::add_github_remote(path, &project.github_owner, &project.github_repo)?;
    local::git::push_all(
        path,
        &project.baseline_branch,
        &project.github_owner,
        &project.github_repo,
    )
}

fn github_push_was_rejected(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("403")
        || error.contains("permission denied")
        || error.contains("write access")
        || error.contains("archived")
        || error.contains("read-only")
        || error.contains("repository not found")
        || error.contains("authentication failed")
        || error.contains("could not read username")
}

async fn create_independent_project_repository(
    mut project: local::model::LocalProject,
) -> Result<local::model::LocalProject> {
    let store = Store::open()?;
    let session_ids = store
        .list_chat_sessions_by_project(&project.id)?
        .into_iter()
        .map(|session| session.id)
        .collect::<Vec<_>>();
    local::git::migrate_legacy_project_worktrees(&project, &session_ids)?;
    let source_repository = project
        .has_github_repository()
        .then(|| (project.github_owner.clone(), project.github_repo.clone()));
    let reroot_shallow = local::git::prepare_shallow_repository_for_publication(
        std::path::Path::new(&project.repo_path),
    )?;
    let (owner, repo, _) = local::github::create_project_repo(&project.slug).await?;
    if reroot_shallow {
        local::git::reroot_shallow_repository(
            std::path::Path::new(&project.repo_path),
            &project.baseline_branch,
            source_repository.as_ref(),
        )?;
    }
    project.github_owner = owner;
    project.github_repo = repo;
    project.github_sync_enabled = false;
    store.update_local_project(&project)?;
    Ok(project)
}

async fn push_project_for_sync(
    mut project: local::model::LocalProject,
) -> Result<local::model::LocalProject> {
    if local::git::resolve_github_token().is_none() {
        return Err(anyhow!(
            "Connect GitHub first with `gh auth login` or a GitHub token."
        ));
    }

    let mut using_existing_repository = project.has_github_repository();
    if using_existing_repository {
        let can_push = local::github::repo_meta(&project.github_owner, &project.github_repo)
            .await
            .is_some_and(|meta| meta.can_push && !meta.archived);
        if !can_push {
            project = create_independent_project_repository(project).await?;
            using_existing_repository = false;
        }
    } else {
        project = create_independent_project_repository(project).await?;
        using_existing_repository = false;
    }

    let push_once = |project: &local::model::LocalProject| {
        let mut project = project.clone();
        project.github_sync_enabled = true;
        tokio::task::spawn_blocking(move || push_project(&project))
    };
    let first_push = push_once(&project)
        .await
        .map_err(|error| anyhow!("Git push task failed: {error}"))?;
    if let Err(error) = first_push {
        if !using_existing_repository || !github_push_was_rejected(&error.to_string()) {
            return Err(error);
        }
        project = create_independent_project_repository(project).await?;
        push_once(&project)
            .await
            .map_err(|error| anyhow!("Git push task failed: {error}"))??;
    }

    project.github_sync_enabled = true;
    Store::open()?.update_local_project(&project)?;
    Ok(project)
}

async fn enable_project_github(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    reject_if_moving(&state)?;
    let _admission = state
        .project_lifecycle
        .admit(&id)
        .ok_or_else(|| bad_request("project deletion is in progress"))?;
    reject_if_moving(&state)?;
    let _lock = project_publication_lock(&state, &id).await;
    let store = Store::open()?;
    let project = store
        .get_local_project(&id)?
        .ok_or_else(|| not_found("project"))?;
    let project = push_project_for_sync(project).await.map_err(bad_request)?;
    let git_status = project_git_json(&project);
    Ok(Json(
        json!({ "project": project_json(&project), "git": git_status }),
    ))
}

async fn disable_project_github(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult {
    reject_if_moving(&state)?;
    let _admission = state
        .project_lifecycle
        .admit(&id)
        .ok_or_else(|| bad_request("project deletion is in progress"))?;
    reject_if_moving(&state)?;
    let _lock = project_publication_lock(&state, &id).await;
    let store = Store::open()?;
    let mut project = store
        .get_local_project(&id)?
        .ok_or_else(|| not_found("project"))?;
    project.github_sync_enabled = false;
    store.update_local_project(&project)?;
    Ok(Json(json!({
        "project": project_json(&project),
        "git": project_git_json(&project),
    })))
}

async fn push_project_github(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    reject_if_moving(&state)?;
    let _admission = state
        .project_lifecycle
        .admit(&id)
        .ok_or_else(|| bad_request("project deletion is in progress"))?;
    reject_if_moving(&state)?;
    let _lock = project_publication_lock(&state, &id).await;
    let project = Store::open()?
        .get_local_project(&id)?
        .ok_or_else(|| not_found("project"))?;
    let project_for_push = project.clone();
    let git_status = tokio::task::spawn_blocking(move || -> Result<Value> {
        push_project(&project_for_push)?;
        Ok(project_git_json(&project_for_push))
    })
    .await
    .map_err(|error| ApiError::from(anyhow!("git task failed: {error}")))?
    .map_err(bad_request)?;
    Ok(Json(
        json!({ "project": project_json(&project), "git": git_status }),
    ))
}

async fn github_account() -> ApiResult {
    Ok(Json(
        json!({ "login": local::github::viewer_login().await }),
    ))
}

#[derive(Deserialize)]
struct ProjectRepoPreviewQuery {
    name: String,
}

async fn github_project_repo_preview(Query(q): Query<ProjectRepoPreviewQuery>) -> ApiResult {
    let candidate = local::projects::project_slug_preview(&Store::open()?, q.name.trim())?;
    let repo = local::github::available_project_repo_name(&candidate).await;
    Ok(Json(json!({ "repo": repo })))
}

#[derive(Deserialize)]
struct RepoAccessQuery {
    owner: String,
    repo: String,
}

async fn github_repo_access(Query(q): Query<RepoAccessQuery>) -> ApiResult {
    let owner = q.owner.trim();
    let repo = q.repo.trim();
    if owner.is_empty() || repo.is_empty() {
        return Err(bad_request("owner and repo are required"));
    }
    let meta = local::github::repo_meta(owner, repo).await;
    Ok(Json(json!({
        "canPush": meta.is_some_and(|meta| meta.can_push && !meta.archived),
    })))
}

/// Mark a project visited: bumps updated_at, which drives the recency sort
/// and the SSE project.updated diff.
async fn open_project(Path(id): Path<String>) -> ApiResult {
    let store = Store::open()?;
    store.touch_local_project(&id)?;
    let project = store
        .get_local_project(&id)?
        .ok_or_else(|| not_found("project"))?;
    Ok(Json(json!({ "project": project_json(&project) })))
}

/// Present-vs-absent for PATCH fields: absent = leave, null = clear.
fn double_option<'de, D>(d: D) -> std::result::Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(d).map(Some)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateProjectReq {
    name: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    run_command: Option<Option<String>>,
}

async fn update_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateProjectReq>,
) -> ApiResult {
    reject_if_moving(&state)?;
    let _admission = state
        .project_lifecycle
        .admit(&id)
        .ok_or_else(|| bad_request("project deletion is in progress"))?;
    reject_if_moving(&state)?;
    if req.name.is_none() && req.run_command.is_none() {
        return Err(bad_request(
            "nothing to update: pass name and/or runCommand",
        ));
    }
    let store = Store::open()?;
    let mut project = store
        .get_local_project(&id)?
        .ok_or_else(|| not_found("project"))?;
    if let Some(name) = req.name {
        if name.trim().is_empty() {
            return Err(bad_request("name cannot be empty"));
        }
        project.name = name.trim().to_string();
    }
    if let Some(cmd) = req.run_command {
        project.run_command = cmd.filter(|c| !c.trim().is_empty());
    }
    store.update_local_project(&project)?;
    // Re-read: update bumps updated_at, which is also what fires the SSE
    // project.updated diff.
    let project = store
        .get_local_project(&id)?
        .ok_or_else(|| not_found("project"))?;
    Ok(Json(json!({ "project": project_json(&project) })))
}

/// Delete a project and everything hanging off it. Refuses while runs are in
/// flight (deleting their rows would strand the supervisor mid-job) — but
/// requests their cancellation, so a retry shortly after goes through. The
/// The registered repository folder is left untouched.
async fn delete_project(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    reject_if_moving(&state)?;
    let _deleting_project = state
        .project_lifecycle
        .begin_delete(&id)
        .ok_or_else(|| bad_request("project has an operation or deletion in progress"))?;
    reject_if_moving(&state)?;
    let store = Store::open()?;
    let project = store
        .get_local_project(&id)?
        .ok_or_else(|| not_found("project"))?;
    let in_flight: Vec<_> = store
        .list_runs_by_project(&id)?
        .into_iter()
        .filter(|r| !is_terminal(&r.status))
        .collect();
    if !in_flight.is_empty() {
        let mut failures = Vec::new();
        for run in &in_flight {
            if let Err(err) = crate::commands::exp::request_local_run_cancel(&store, &run.id) {
                failures.push(format!("{}: {err}", run.id));
            }
        }
        if !failures.is_empty() {
            let requested = in_flight.len() - failures.len();
            return Err(bad_request(format!(
                "{requested} run(s) cancellation requested; cancellation failed for {}",
                failures.join(", ")
            )));
        }
        return Err(bad_request(format!(
            "{} run(s) still in flight — cancellation requested; retry once they stop",
            in_flight.len()
        )));
    }
    // Abort any in-flight chat turns before their rows disappear, and clean up
    // each session's serve child + worktree (the rows cascade with the project).
    let sessions = store.list_chat_sessions_by_project(&id)?;
    let mut _session_deletions = Vec::with_capacity(sessions.len());
    for session in &sessions {
        _session_deletions.push(
            state
                .chat
                .begin_session_delete(&session.id)
                .ok_or_else(|| bad_request("a chat session deletion is already in progress"))?,
        );
    }
    for session in &sessions {
        state.chat.clear_queue(&session.id)?;
        let _ = state.chat.interrupt(&session.id).await;
        state.chat.opencode.kill_session(&session.id).await;
        state.chat.codex.kill_session(&session.id).await;
        state.chat.claude.forget_session(&session.id).await;
    }
    store.delete_local_project(&id)?;
    for session in &sessions {
        local::chat::cleanup_session_transcript_artifacts(&session.id);
        local::chat::cleanup_session_worktree(&project, &session.id);
    }
    Ok(Json(json!({ "ok": true })))
}

async fn list_experiments(Path(id): Path<String>) -> ApiResult {
    let store = Store::open()?;
    store
        .get_local_project(&id)?
        .ok_or_else(|| not_found("project"))?;
    let experiments = store.list_experiments_by_project(&id)?;
    Ok(Json(json!({ "experiments": experiments })))
}

async fn list_project_runs(Path(id): Path<String>) -> ApiResult {
    let store = Store::open()?;
    store
        .get_local_project(&id)?
        .ok_or_else(|| not_found("project"))?;
    let runs: Vec<ApiRun> = store
        .list_runs_by_project(&id)?
        .iter()
        .map(ApiRun::from)
        .collect();
    Ok(Json(json!({ "runs": runs })))
}

async fn compute_backends() -> Json<Value> {
    Json(json!({ "backends": crate::compute::capabilities() }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateRunReq {
    experiment_id: String,
    backend: Option<String>,
    flavor: Option<String>,
    host: Option<String>,
    manifest: Option<String>,
    image: Option<String>,
    timeout: Option<String>,
    org: Option<String>,
    provider: Option<String>,
    disk: Option<i64>,
    #[serde(default)]
    force: bool,
}

async fn create_run(State(state): State<AppState>, Json(req): Json<CreateRunReq>) -> ApiResult {
    reject_if_moving(&state)?;
    let store = Store::open()?;
    let experiment = store
        .get_local_experiment(&req.experiment_id)?
        .ok_or_else(|| not_found("experiment"))?;
    let _admission = state
        .project_lifecycle
        .admit(&experiment.project_id)
        .ok_or_else(|| bad_request("project deletion is in progress"))?;
    let mut backend = req.backend;
    let mut flavor = req.flavor;
    local::apply_compute_default(&mut backend, &mut flavor);
    let args = crate::ExpRunArgs {
        exp_id: req.experiment_id,
        disk: req.disk,
        provider: req.provider,
        backend: Some(backend.unwrap_or_else(|| "local".to_string())),
        flavor,
        org: req.org,
        host: req.host,
        manifest: req.manifest,
        image: req.image,
        timeout: req.timeout,
        force: req.force,
    };
    let run = crate::compute::submit(&args).await.map_err(bad_request)?;
    Ok(Json(json!({ "run": ApiRun::from(&run) })))
}

fn backend_for_run(
    run: &StoredRun,
) -> std::result::Result<Box<dyn crate::compute::ComputeBackend>, ApiError> {
    let descriptor =
        crate::jobs::BackendDescriptor::parse(&run.backend_json).map_err(bad_request)?;
    let id = descriptor
        .kind
        .strip_suffix("_job")
        .unwrap_or(&descriptor.kind);
    let id = if id == "k8s" { "k8s" } else { id };
    crate::compute::backend(id).map_err(bad_request)
}

async fn get_run(Path(id): Path<String>) -> ApiResult {
    let run = Store::open()?
        .get_run(&id)?
        .ok_or_else(|| not_found("run"))?;
    let backend = backend_for_run(&run)?;
    let run = backend.status(&run).await.map_err(bad_request)?;
    if is_terminal(&run.status) {
        backend.cleanup(&run).await.map_err(bad_request)?;
    }
    Ok(Json(json!({ "run": ApiRun::from(&run) })))
}

/// Newest-first cap for the cross-project instances list. Generous: the store
/// is a local single-user SQLite db, so this only bounds pathological history.
const INSTANCES_LIMIT: usize = 500;

/// Every run across all projects (running first on the client), each tagged
/// with its owning project's name — the "instances" view of compute the agents
/// have spun up (Modal / HF / SSH / K8s), regardless of which project launched
/// it. Includes finished runs as history; the client surfaces live ones first.
async fn list_instances() -> ApiResult {
    let store = Store::open()?;
    let names: HashMap<String, String> = store
        .list_local_projects()?
        .into_iter()
        .map(|p| (p.id, p.name))
        .collect();
    let mut instances: Vec<Value> = Vec::new();
    for run in store.list_runs(INSTANCES_LIMIT)? {
        // ApiRun is a plain serializable struct, so this can't realistically
        // fail; propagate rather than emit a malformed row if it ever does.
        let mut value = serde_json::to_value(ApiRun::from(&run))
            .map_err(|e| anyhow!("serialize run {}: {e}", run.id))?;
        if let (Some(obj), Some(name)) = (value.as_object_mut(), names.get(&run.project_id)) {
            obj.insert("projectName".into(), json!(name));
        }
        instances.push(value);
    }
    Ok(Json(json!({ "instances": instances })))
}

async fn cancel_run(Path(id): Path<String>) -> ApiResult {
    let store = Store::open()?;
    let run = local::local_run(&store, &id)?.ok_or_else(|| not_found("run"))?;
    // A terminal run must not gain a stale cancel_requested flag.
    if is_terminal(&run.status) {
        return Err(bad_request(format!("run already {}", run.status)));
    }
    let backend = backend_for_run(&run)?;
    backend.cancel(&run).await.map_err(bad_request)?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct LogsQuery {
    cursor: Option<u64>,
}

async fn run_logs(Path(id): Path<String>, Query(q): Query<LogsQuery>) -> ApiResult {
    let run = Store::open()?
        .get_run(&id)?
        .ok_or_else(|| not_found("run"))?;
    let batch = backend_for_run(&run)?
        .logs(&run, crate::compute::LogCursor(q.cursor.unwrap_or(0)))
        .await
        .map_err(bad_request)?;
    Ok(Json(json!(batch)))
}

#[derive(Deserialize)]
struct LogQuery {
    offset: Option<u64>,
}

async fn run_log(Path(id): Path<String>, Query(q): Query<LogQuery>) -> ApiResult {
    let store = Store::open()?;
    store.get_run(&id)?.ok_or_else(|| not_found("run"))?;
    let offset = q.offset.unwrap_or(0);
    let chunk = read_log_from(&id, offset, 4_000_000);
    let next_offset = offset + chunk.len() as u64;
    Ok(Json(json!({
        "dataBase64": base64::engine::general_purpose::STANDARD.encode(&chunk),
        "nextOffset": next_offset,
        "eof": next_offset >= log_size(&id),
    })))
}

// --- diffs ------------------------------------------------------------------
//
// Same payload shape as the OpenResearch api diff endpoints:
// `{diff, truncated, bytesRead, byteLimit}` with the raw unified-diff text.
// All of these shell out to git against the project's local clone, so they
// run on the blocking pool.

fn diff_json(d: local::git::DiffPayload) -> Value {
    json!({
        "diff": d.diff,
        "truncated": d.truncated,
        "bytesRead": d.bytes_read,
        "byteLimit": local::git::MAX_DIFF_BYTES,
    })
}

/// Off-worker helper for git-backed handlers.
async fn blocking_api<F>(f: F) -> ApiResult
where
    F: FnOnce() -> std::result::Result<Json<Value>, ApiError> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::from(anyhow!("git task failed: {e}")))?
}

/// Cumulative diff of a run's commit vs its experiment's parent branch. A root
/// experiment on a distinct branch compares against the project's baseline.
async fn run_diff(Path(id): Path<String>) -> ApiResult {
    blocking_api(move || {
        let store = Store::open()?;
        let run = store.get_run(&id)?.ok_or_else(|| not_found("run"))?;
        let sha = run
            .commit_sha
            .clone()
            .ok_or_else(|| bad_request("run has no commit to diff"))?;
        let exp = store
            .get_local_experiment(&run.experiment_id)?
            .ok_or_else(|| not_found("experiment"))?;
        let project = store
            .get_local_project(&exp.project_id)?
            .ok_or_else(|| not_found("project"))?;
        let base = match exp.parent_experiment_id {
            Some(parent_id) => {
                store
                    .get_local_experiment(&parent_id)?
                    .ok_or_else(|| not_found("parent experiment"))?
                    .branch_name
            }
            None => project.baseline_branch.clone(),
        };
        let repo = std::path::Path::new(&project.repo_path);
        let payload = local::git::diff_range(repo, &base, &sha)?;
        Ok(Json(diff_json(payload)))
    })
    .await
}

/// Cumulative committed diff of an experiment branch vs its parent branch.
async fn experiment_diff(Path(id): Path<String>) -> ApiResult {
    blocking_api(move || {
        let store = Store::open()?;
        let exp = store
            .get_local_experiment(&id)?
            .ok_or_else(|| not_found("experiment"))?;
        let project = store
            .get_local_project(&exp.project_id)?
            .ok_or_else(|| not_found("project"))?;
        let base = match &exp.parent_experiment_id {
            Some(parent_id) => {
                store
                    .get_local_experiment(parent_id)?
                    .ok_or_else(|| not_found("parent experiment"))?
                    .branch_name
            }
            None => project.baseline_branch.clone(),
        };
        let repo = std::path::Path::new(&project.repo_path);
        Ok(Json(diff_json(local::git::diff_range(
            repo,
            &base,
            &exp.branch_name,
        )?)))
    })
    .await
}

/// Commits on the experiment branch: child experiments list `parent..branch`,
/// the baseline lists the branch's recent history.
async fn experiment_commits(Path(id): Path<String>) -> ApiResult {
    blocking_api(move || {
        let store = Store::open()?;
        let exp = store
            .get_local_experiment(&id)?
            .ok_or_else(|| not_found("experiment"))?;
        let project = store
            .get_local_project(&exp.project_id)?
            .ok_or_else(|| not_found("project"))?;
        let repo = std::path::Path::new(&project.repo_path);
        let commits = match &exp.parent_experiment_id {
            Some(pid) => {
                let parent = store
                    .get_local_experiment(pid)?
                    .ok_or_else(|| not_found("parent experiment"))?;
                local::git::list_commits_between(repo, &parent.branch_name, &exp.branch_name, 100)?
            }
            None if exp.branch_name != project.baseline_branch => local::git::list_commits_between(
                repo,
                &project.baseline_branch,
                &exp.branch_name,
                100,
            )?,
            None => local::git::list_commits(repo, &exp.branch_name, 25)?,
        };
        let commits: Vec<Value> = commits
            .iter()
            .map(|c| json!({ "sha": c.sha, "subject": c.subject, "committedAt": c.committed_at }))
            .collect();
        Ok(Json(json!({ "commits": commits })))
    })
    .await
}

async fn experiment_commit_diff(Path((id, sha)): Path<(String, String)>) -> ApiResult {
    if !sha.chars().all(|c| c.is_ascii_hexdigit()) || sha.len() < 7 || sha.len() > 64 {
        return Err(bad_request("invalid commit sha"));
    }
    blocking_api(move || {
        let store = Store::open()?;
        let exp = store
            .get_local_experiment(&id)?
            .ok_or_else(|| not_found("experiment"))?;
        let project = store
            .get_local_project(&exp.project_id)?
            .ok_or_else(|| not_found("project"))?;
        let repo = std::path::Path::new(&project.repo_path);
        let payload = local::git::commit_diff(repo, &sha)?;
        Ok(Json(diff_json(payload)))
    })
    .await
}

/// Live uncommitted changes in the project's clone (the agent's working
/// tree), mapped back to the experiment whose branch is checked out.
async fn project_working_tree(Path(id): Path<String>) -> ApiResult {
    blocking_api(move || {
        let store = Store::open()?;
        let project = store
            .get_local_project(&id)?
            .ok_or_else(|| not_found("project"))?;
        let repo = std::path::Path::new(&project.repo_path);
        let (branch, payload) = local::git::working_tree_diff(repo)?;
        let experiment_id = match &branch {
            Some(b) => store
                .list_experiments_by_project(&project.id)?
                .into_iter()
                .find(|e| &e.branch_name == b)
                .map(|e| e.id),
            None => None,
        };
        Ok(Json(json!({
            "branch": branch,
            "experimentId": experiment_id,
            "diff": payload.diff,
            "truncated": payload.truncated,
        })))
    })
    .await
}

/// Live view of one chat session's private worktree — what the agent has
/// changed, before any run exists. Unlike `project_working_tree` (clone-scoped,
/// diffed against HEAD), the session worktree starts detached on the baseline
/// tip and the agent commits to experiment branches, so "what it changed" is
/// the working tree diffed against the merge-base of the baseline and HEAD; a
/// bare HEAD diff would hide every committed edit. Read-only throughout: no
/// index-touching (`git add -N`) that would mutate the agent's checkout.
///
/// A never-started session (worktree is lazy) or a pruned worktree degrades to
/// `resolve_checkout_root`'s clone fallback; we report `{ exists: false }`
/// rather than pass off the clone's contents as the session's work.
async fn session_worktree(Path(id): Path<String>) -> ApiResult {
    blocking_api(move || {
        let store = Store::open()?;
        let session = store
            .get_chat_session(&id)?
            .ok_or_else(|| not_found("chat session"))?;
        let project = store
            .get_local_project(&session.project_id)?
            .ok_or_else(|| not_found("project"))?;
        let (root, root_kind) = resolve_checkout_root(&store, &project, Some(&id))?;
        if root_kind != "worktree" {
            return Ok(Json(json!({ "exists": false })));
        }
        let branch = local::git::current_branch(&root);
        // Diff against the merge-base of the baseline tip and HEAD — the fork
        // point of the agent's work. Every step that can't resolve (unrelated
        // histories, unborn HEAD) falls back to HEAD, so
        // the diff degrades to "uncommitted only" rather than erroring.
        let baseline = &project.baseline_branch;
        let base =
            local::git::merge_base(&root, baseline, "HEAD")?.unwrap_or_else(|| "HEAD".to_string());
        let files = local::git::changed_files(&root, &base)?;
        let payload = local::git::working_tree_diff_against(&root, Some(&base))?;
        Ok(Json(json!({
            "exists": true,
            "branch": branch,
            "baselineBranch": baseline,
            "baseSha": base,
            "files": files,
            "diff": diff_json(payload),
        })))
    })
    .await
}

/// Cap on file bytes served to the viewer (mirrors openresearch.sh).
const FILE_READ_LIMIT: u64 = 512_000;

/// Resolve which on-disk checkout answers a file/code request for a project.
///
/// The chat session's worktree is where the agent actually works, so it can
/// hold files the hub clone's checkout never sees. When `session_id` is given
/// it must be this project's session (the authorization boundary, which also
/// pins the worktree dir to a store-issued id); a missing worktree (pruned, or
/// never created) degrades to the clone rather than erroring, but any other
/// worktree failure is reported, not papered over. Returns the canonicalized
/// root and whether it is the `"worktree"` or the `"clone"`.
fn resolve_checkout_root(
    store: &Store,
    project: &local::model::LocalProject,
    session_id: Option<&str>,
) -> std::result::Result<(std::path::PathBuf, &'static str), ApiError> {
    let session_id = session_id.map(str::trim).filter(|s| !s.is_empty());
    let worktree = match session_id {
        Some(s) => {
            let session = store
                .get_chat_session(s)?
                .filter(|sess| sess.project_id == project.id)
                .ok_or_else(|| not_found("chat session"))?;
            let dir = local::git::existing_session_worktree_path(project, &session.id);
            match std::fs::canonicalize(&dir) {
                Ok(p) => Some(p),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(ApiError::from(anyhow!("session worktree unavailable: {e}"))),
            }
        }
        None => None,
    };
    match worktree {
        Some(r) => Ok((r, "worktree")),
        None => Ok((
            std::fs::canonicalize(&project.repo_path)
                .map_err(|e| ApiError::from(anyhow!("repo clone unavailable: {e}")))?,
            "clone",
        )),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodeTreeQuery {
    /// Branch to list the committed tree of; absent lists a live checkout.
    r#ref: Option<String>,
    /// Chat session whose worktree to list (the live view the Worktree tab's
    /// Files pane wants). Absent falls back to the hub clone's checkout.
    /// Mutually exclusive with `ref` — a committed tree has no live worktree.
    session_id: Option<String>,
}

/// Cap on entries returned by the code-tree listing.
const CODE_TREE_LIMIT: usize = 20_000;

/// Flat file listing for the UI code browser. With `ref`: the committed tree
/// of that branch (local ref first, then origin's), independent of any
/// checkout. Without: the hub clone's checkout via `git ls-files`, so
/// gitignored trees are excluded and untracked-but-new files are included.
/// Paths are repo-relative; the client builds the nested tree.
async fn project_code_tree(Path(id): Path<String>, Query(q): Query<CodeTreeQuery>) -> ApiResult {
    blocking_api(move || {
        let store = Store::open()?;
        let project = store
            .get_local_project(&id)?
            .ok_or_else(|| not_found("project"))?;
        let ref_name = q.r#ref.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let session_id = q
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if ref_name.is_some() && session_id.is_some() {
            return Err(bad_request("ref and sessionId are mutually exclusive"));
        }
        // Branch refs live in the shared object DB — any checkout resolves them;
        // for a live listing the session's worktree is the live view when given
        // (its untracked files the clone never sees), else the hub clone.
        let (root, root_kind) = resolve_checkout_root(&store, &project, session_id)?;
        let (root_kind, branch, mut entries) = match ref_name {
            Some(name) => {
                let sha = local::git::resolve_branch_commit(&root, name)?
                    .ok_or_else(|| not_found("branch"))?;
                let entries = local::git::list_tree_files(&root, &sha)?;
                ("branch", Some(name.to_string()), entries)
            }
            None => {
                let branch = local::git::current_branch(&root);
                let entries = local::git::list_worktree_files(&root)?;
                (root_kind, branch, entries)
            }
        };
        entries.sort();
        // During a merge conflict `ls-files --cached` emits an unmerged path
        // once per stage — collapse to one entry (they'd be duplicate keys).
        entries.dedup();
        let truncated = entries.len() > CODE_TREE_LIMIT;
        entries.truncate(CODE_TREE_LIMIT);
        Ok(Json(json!({
            "root": root_kind,
            "branch": branch,
            "entries": entries,
            "truncated": truncated,
        })))
    })
    .await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectFileQuery {
    path: String,
    /// Chat session whose worktree holds the file. Absent (or the worktree
    /// already pruned) falls back to the hub clone. Ignored when `ref` is given.
    session_id: Option<String>,
    /// Branch to read the committed file from, instead of a live checkout.
    r#ref: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectFileResponse {
    path: String,
    content: String,
    truncated: bool,
    binary: bool,
    not_found: bool,
    root: &'static str,
    presentation: local::files::FilePresentation,
}

impl ProjectFileResponse {
    fn missing(
        path: String,
        root: &'static str,
        presentation: local::files::FilePresentation,
    ) -> Self {
        Self {
            path,
            content: String::new(),
            truncated: false,
            binary: false,
            not_found: true,
            root,
            presentation,
        }
    }

    fn non_text(
        path: String,
        root: &'static str,
        presentation: local::files::FilePresentation,
    ) -> Self {
        Self {
            path,
            content: String::new(),
            truncated: false,
            binary: true,
            not_found: false,
            root,
            presentation,
        }
    }

    fn text(
        path: String,
        root: &'static str,
        content: String,
        truncated: bool,
        binary: bool,
        presentation: local::files::FilePresentation,
    ) -> Self {
        Self {
            path,
            content,
            truncated,
            binary,
            not_found: false,
            root,
            presentation,
        }
    }
}

fn validated_project_file_path(
    path: &str,
) -> std::result::Result<(String, std::path::PathBuf), ApiError> {
    let rel = path.trim().trim_start_matches("./").to_string();
    if rel.is_empty() || rel.len() > 1024 {
        return Err(bad_request("invalid path"));
    }
    let rel_path = std::path::PathBuf::from(&rel);
    let traversal = rel_path.is_absolute()
        || rel_path
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)));
    if traversal {
        return Err(bad_request("path must be repo-relative"));
    }
    Ok((rel, rel_path))
}

fn decode_project_file_text(bytes: Vec<u8>, truncated: bool) -> (String, bool) {
    if bytes.contains(&0) {
        return (String::new(), true);
    }
    match String::from_utf8(bytes) {
        Ok(content) => (content, false),
        Err(error) if truncated && error.utf8_error().error_len().is_none() => {
            let valid_up_to = error.utf8_error().valid_up_to();
            let mut bytes = error.into_bytes();
            bytes.truncate(valid_up_to);
            (String::from_utf8(bytes).unwrap_or_default(), false)
        }
        Err(_) => (String::new(), true),
    }
}

/// One file for the UI file viewer. With `ref`: the committed content on that
/// branch (a streamed, capped `git cat-file` read), independent of any
/// checkout. Without: the project's checkout — the chat session's worktree
/// when `sessionId` is given, else the hub clone. Path is repo-relative;
/// traversal outside the checkout is rejected. The response's `root` says
/// which source actually answered, so the UI can flag fallback.
async fn project_file(
    Path(id): Path<String>,
    Query(q): Query<ProjectFileQuery>,
) -> std::result::Result<Json<ProjectFileResponse>, ApiError> {
    tokio::task::spawn_blocking(move || {
        use std::io::Read as _;
        let (rel, rel_path) = validated_project_file_path(&q.path)?;

        let store = Store::open()?;
        let project = store
            .get_local_project(&id)?
            .ok_or_else(|| not_found("project"))?;
        let presentation = local::files::presentation_for_path(&rel);
        let ref_name = q.r#ref.as_deref().map(str::trim).filter(|s| !s.is_empty());
        if let Some(name) = ref_name {
            let (root, _) = resolve_checkout_root(&store, &project, None)?;
            let sha = local::git::resolve_branch_commit(&root, name)?
                .ok_or_else(|| not_found("branch"))?;
            if !matches!(
                presentation,
                local::files::FilePresentation::Text | local::files::FilePresentation::Unknown
            ) {
                return if local::git::file_size_at(&root, &sha, &rel)?.is_some() {
                    Ok(Json(ProjectFileResponse::non_text(
                        rel,
                        "branch",
                        presentation,
                    )))
                } else {
                    Ok(Json(ProjectFileResponse::missing(
                        rel,
                        "branch",
                        presentation,
                    )))
                };
            }
            // Streamed + capped: a committed multi-GB blob must not become a
            // multi-GB allocation. Missing path is an exit-code check inside
            // the helper (`cat-file -e`) — no error-message parsing.
            return match local::git::file_bytes_at_capped(&root, &sha, &rel, FILE_READ_LIMIT)? {
                Some((bytes, truncated)) => {
                    let (content, binary) = decode_project_file_text(bytes, truncated);
                    let presentation = if binary {
                        local::files::FilePresentation::Download
                    } else {
                        local::files::FilePresentation::Text
                    };
                    Ok(Json(ProjectFileResponse::text(
                        rel,
                        "branch",
                        content,
                        truncated,
                        binary,
                        presentation,
                    )))
                }
                None => Ok(Json(ProjectFileResponse::missing(
                    rel,
                    "branch",
                    presentation,
                ))),
            };
        }
        let (root, root_kind) = resolve_checkout_root(&store, &project, q.session_id.as_deref())?;
        // Canonicalize so symlinks can't escape the checkout.
        let full = match std::fs::canonicalize(root.join(&rel_path)) {
            Ok(p) => p,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Json(ProjectFileResponse::missing(
                    rel,
                    root_kind,
                    presentation,
                )))
            }
            Err(e) => return Err(ApiError::from(anyhow!("read failed: {e}"))),
        };
        if !full.starts_with(&root) {
            return Err(bad_request("path escapes repository"));
        }
        if full.is_dir() {
            return Err(bad_request("path is a directory"));
        }
        if !matches!(
            presentation,
            local::files::FilePresentation::Text | local::files::FilePresentation::Unknown
        ) {
            return Ok(Json(ProjectFileResponse::non_text(
                rel,
                root_kind,
                presentation,
            )));
        }
        let file = match std::fs::File::open(&full) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Json(ProjectFileResponse::missing(
                    rel,
                    root_kind,
                    presentation,
                )))
            }
            Err(e) => return Err(ApiError::from(anyhow!("read failed: {e}"))),
        };
        let mut buf = Vec::new();
        std::io::Read::take(file, FILE_READ_LIMIT + 1)
            .read_to_end(&mut buf)
            .map_err(|e| ApiError::from(anyhow!("read failed: {e}")))?;
        let truncated = buf.len() as u64 > FILE_READ_LIMIT;
        buf.truncate(FILE_READ_LIMIT as usize);
        let (content, binary) = decode_project_file_text(buf, truncated);
        let presentation = if binary {
            local::files::FilePresentation::Download
        } else {
            local::files::FilePresentation::Text
        };
        Ok(Json(ProjectFileResponse::text(
            rel,
            root_kind,
            content,
            truncated,
            binary,
            presentation,
        )))
    })
    .await
    .map_err(|e| ApiError::from(anyhow!("file task failed: {e}")))?
}

/// Upper bound on a single save — generous room to grow past the read cap while
/// still bounding one write.
const FILE_WRITE_LIMIT: u64 = 8 * 1024 * 1024;

/// True when a repo-relative path steps into the `.git` metadata dir — writing
/// there (`config`, `hooks/*`) is an arbitrary-command vector. Case-insensitive
/// so `.GIT` can't slip past on macOS/Windows.
fn touches_git_dir(rel_path: &std::path::Path) -> bool {
    rel_path.components().any(
        |c| matches!(c, std::path::Component::Normal(name) if name.eq_ignore_ascii_case(".git")),
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WriteProjectFileReq {
    path: String,
    content: String,
    /// Chat session whose worktree owns the file; absent writes the hub clone.
    session_id: Option<String>,
}

/// Overwrite an existing text file in the project's live checkout with edited
/// content. Only the worktree/clone is writable — committed branch trees have no
/// `ref` path here and stay read-only. Traversal and symlink escapes are
/// rejected by canonicalizing the target and confirming it stays under the root.
async fn write_project_file(
    Path(id): Path<String>,
    Json(req): Json<WriteProjectFileReq>,
) -> ApiResult {
    blocking_api(move || {
        let (rel, rel_path) = validated_project_file_path(&req.path)?;
        if touches_git_dir(&rel_path) {
            return Err(bad_request("cannot edit files under .git"));
        }
        if req.content.len() as u64 > FILE_WRITE_LIMIT {
            return Err(bad_request("file too large to save"));
        }
        if !matches!(
            local::files::presentation_for_path(&rel),
            local::files::FilePresentation::Text | local::files::FilePresentation::Unknown
        ) {
            return Err(bad_request("not an editable text file"));
        }
        let store = Store::open()?;
        let project = store
            .get_local_project(&id)?
            .ok_or_else(|| not_found("project"))?;
        let (root, root_kind) = resolve_checkout_root(&store, &project, req.session_id.as_deref())?;
        // A session write that fell back to the clone means the worktree was
        // pruned mid-edit — refuse rather than silently write another checkout.
        if req.session_id.is_some() && root_kind == "clone" {
            return Err(bad_request(
                "this session's worktree is no longer available — reload the file",
            ));
        }
        // Canonicalize the existing target so a symlinked path can't escape the
        // checkout; a missing file means the editor's copy is stale.
        let full = match std::fs::canonicalize(root.join(&rel_path)) {
            Ok(p) => p,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(not_found("file")),
            Err(e) => return Err(ApiError::from(anyhow!("save failed: {e}"))),
        };
        if !full.starts_with(&root) {
            return Err(bad_request("path escapes repository"));
        }
        if full.is_dir() {
            return Err(bad_request("path is a directory"));
        }
        std::fs::write(&full, req.content.as_bytes())
            .map_err(|e| ApiError::from(anyhow!("save failed: {e}")))?;
        Ok(Json(json!({
            "ok": true,
            "root": root_kind,
            "bytesWritten": req.content.len(),
        })))
    })
    .await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenProjectFileReq {
    path: String,
    session_id: Option<String>,
}

/// Open a checkout file in the machine's default app for its type (the user's
/// editor for source files). Resolves the same worktree/clone the reader uses
/// and confirms the file is inside it before handing the path to the OS opener.
async fn open_project_file(
    Path(id): Path<String>,
    Json(req): Json<OpenProjectFileReq>,
) -> ApiResult {
    blocking_api(move || {
        let (_, rel_path) = validated_project_file_path(&req.path)?;
        if touches_git_dir(&rel_path) {
            return Err(bad_request("cannot open files under .git"));
        }
        let store = Store::open()?;
        let project = store
            .get_local_project(&id)?
            .ok_or_else(|| not_found("project"))?;
        let (root, _) = resolve_checkout_root(&store, &project, req.session_id.as_deref())?;
        let full = match std::fs::canonicalize(root.join(&rel_path)) {
            Ok(p) => p,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(not_found("file")),
            Err(e) => return Err(ApiError::from(anyhow!("open failed: {e}"))),
        };
        if !full.starts_with(&root) {
            return Err(bad_request("path escapes repository"));
        }
        if full.is_dir() {
            return Err(bad_request("path is a directory"));
        }
        crate::editors::open_in_default_app(&full)
            .map_err(|e| ApiError::from(anyhow!("could not open file: {e}")))?;
        Ok(Json(json!({ "ok": true })))
    })
    .await
}

enum RawProjectFileSource {
    Disk(std::fs::File),
    Git {
        repo: std::path::PathBuf,
        spec: String,
        size: u64,
    },
}

/// Byte-exact checkout file for browser-native media previews. It resolves the
/// same worktree/clone/branch source as `project_file`, but streams instead of
/// decoding or buffering the file in the API process.
async fn project_raw_file(
    Path(id): Path<String>,
    Query(q): Query<ProjectFileQuery>,
    method: Method,
    headers: HeaderMap,
) -> std::result::Result<Response, ApiError> {
    let source = tokio::task::spawn_blocking(move || {
        let (rel, rel_path) = validated_project_file_path(&q.path)?;
        let store = Store::open()?;
        let project = store
            .get_local_project(&id)?
            .ok_or_else(|| not_found("project"))?;
        let ref_name = q.r#ref.as_deref().map(str::trim).filter(|s| !s.is_empty());
        if let Some(name) = ref_name {
            let (root, _) = resolve_checkout_root(&store, &project, None)?;
            let sha = local::git::resolve_branch_commit(&root, name)?
                .ok_or_else(|| not_found("branch"))?;
            let size =
                local::git::file_size_at(&root, &sha, &rel)?.ok_or_else(|| not_found("file"))?;
            return Ok((
                rel.clone(),
                RawProjectFileSource::Git {
                    repo: root,
                    spec: format!("{sha}:{rel}"),
                    size,
                },
            ));
        }

        let (root, _) = resolve_checkout_root(&store, &project, q.session_id.as_deref())?;
        let full = std::fs::canonicalize(root.join(rel_path)).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                not_found("file")
            } else {
                ApiError::from(anyhow!("read failed: {e}"))
            }
        })?;
        if !full.starts_with(&root) {
            return Err(bad_request("path escapes repository"));
        }
        if full.is_dir() {
            return Err(bad_request("path is a directory"));
        }
        let file =
            std::fs::File::open(&full).map_err(|e| ApiError::from(anyhow!("read failed: {e}")))?;
        Ok((rel, RawProjectFileSource::Disk(file)))
    })
    .await
    .map_err(|e| ApiError::from(anyhow!("file task failed: {e}")))??;

    let (display_path, source) = source;
    let presentation = local::files::presentation_for_path(&display_path);
    match source {
        RawProjectFileSource::Disk(file) => crate::commands::file_serve::disk_response(
            &display_path,
            file,
            presentation,
            &method,
            &headers,
            "no-cache",
        )
        .await
        .map_err(ApiError::from),
        RawProjectFileSource::Git { repo, spec, size } => {
            crate::commands::file_serve::git_response(
                &display_path,
                repo,
                spec,
                size,
                presentation,
                &method,
                &headers,
            )
            .await
            .map_err(ApiError::from)
        }
    }
}

#[derive(Deserialize)]
struct AbsoluteFileQuery {
    path: String,
}

/// Validate an absolute-path request and resolve a leading `~`/`~/` to the home
/// dir (the shell never expands it for us, and agents inline `~/…` paths). The
/// display string stays as typed — it's what the tab shows and what the agent
/// wrote; only the returned `PathBuf` is home-expanded. `~otheruser` is left
/// alone and simply fails the absolute check. The bind is loopback-only and the
/// server runs as the user, so any file the user could read is fair game — this
/// only rejects malformed input, not location.
fn validated_absolute_file_path(
    path: &str,
) -> std::result::Result<(String, std::path::PathBuf), ApiError> {
    let trimmed = path.trim();
    if trimmed.is_empty() || trimmed.len() > 4096 {
        return Err(bad_request("invalid path"));
    }
    let resolved = match trimmed.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => dirs::home_dir()
            .ok_or_else(|| bad_request("no home directory"))?
            .join(rest.trim_start_matches('/')),
        _ => std::path::PathBuf::from(trimmed),
    };
    if !resolved.is_absolute() {
        return Err(bad_request("path must be absolute"));
    }
    Ok((trimmed.to_string(), resolved))
}

/// One file by absolute path, for the UI file viewer — the escape hatch for a
/// file an agent references that lives outside the project's checkout and
/// artifacts (e.g. `/Users/me/.ssh/config`). Same decoded/capped body shape as
/// `project_file`; `root: "abs"`. Loopback-only and no auth, so it reads
/// whatever the user running `orx up` can read — matching how the raw variant
/// and the OS-open endpoint already expose the local disk.
async fn absolute_file(
    Query(q): Query<AbsoluteFileQuery>,
) -> std::result::Result<Json<ProjectFileResponse>, ApiError> {
    tokio::task::spawn_blocking(move || {
        use std::io::Read as _;
        let (display, abs) = validated_absolute_file_path(&q.path)?;
        let presentation = local::files::presentation_for_path(&display);
        let full = match std::fs::canonicalize(&abs) {
            Ok(p) => p,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Json(ProjectFileResponse::missing(
                    display,
                    "abs",
                    presentation,
                )))
            }
            Err(e) => return Err(ApiError::from(anyhow!("read failed: {e}"))),
        };
        if full.is_dir() {
            return Err(bad_request("path is a directory"));
        }
        if !matches!(
            presentation,
            local::files::FilePresentation::Text | local::files::FilePresentation::Unknown
        ) {
            return Ok(Json(ProjectFileResponse::non_text(
                display,
                "abs",
                presentation,
            )));
        }
        let file = match std::fs::File::open(&full) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Json(ProjectFileResponse::missing(
                    display,
                    "abs",
                    presentation,
                )))
            }
            Err(e) => return Err(ApiError::from(anyhow!("read failed: {e}"))),
        };
        let mut buf = Vec::new();
        std::io::Read::take(file, FILE_READ_LIMIT + 1)
            .read_to_end(&mut buf)
            .map_err(|e| ApiError::from(anyhow!("read failed: {e}")))?;
        let truncated = buf.len() as u64 > FILE_READ_LIMIT;
        buf.truncate(FILE_READ_LIMIT as usize);
        let (content, binary) = decode_project_file_text(buf, truncated);
        let presentation = if binary {
            local::files::FilePresentation::Download
        } else {
            local::files::FilePresentation::Text
        };
        Ok(Json(ProjectFileResponse::text(
            display,
            "abs",
            content,
            truncated,
            binary,
            presentation,
        )))
    })
    .await
    .map_err(|e| ApiError::from(anyhow!("file task failed: {e}")))?
}

/// Byte-exact absolute-path file for browser-native media previews and
/// downloads — the streamed counterpart to `absolute_file`, mirroring
/// `project_raw_file` for arbitrary on-disk paths.
async fn absolute_raw_file(
    Query(q): Query<AbsoluteFileQuery>,
    method: Method,
    headers: HeaderMap,
) -> std::result::Result<Response, ApiError> {
    let (display, file) = tokio::task::spawn_blocking(
        move || -> std::result::Result<(String, std::fs::File), ApiError> {
            let (display, abs) = validated_absolute_file_path(&q.path)?;
            let full = std::fs::canonicalize(&abs).map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    not_found("file")
                } else {
                    ApiError::from(anyhow!("read failed: {e}"))
                }
            })?;
            if full.is_dir() {
                return Err(bad_request("path is a directory"));
            }
            let file = std::fs::File::open(&full)
                .map_err(|e| ApiError::from(anyhow!("read failed: {e}")))?;
            Ok((display, file))
        },
    )
    .await
    .map_err(|e| ApiError::from(anyhow!("file task failed: {e}")))??;
    let presentation = local::files::presentation_for_path(&display);
    crate::commands::file_serve::disk_response(
        &display,
        file,
        presentation,
        &method,
        &headers,
        "no-cache",
    )
    .await
    .map_err(ApiError::from)
}

// --- project artifacts ----------------------------------------------------

/// Listing of the project's artifacts dir — the filesystem is the source of
/// truth; this scans it fresh on every call (and creates it if missing).
async fn list_artifacts(Path(id): Path<String>) -> ApiResult {
    blocking_api(move || {
        let store = Store::open()?;
        let project = store
            .get_local_project(&id)?
            .ok_or_else(|| not_found("project"))?;
        let listing = local::files::list(&project)?;
        Ok(Json(json!(listing)))
    })
    .await
}

#[derive(Deserialize)]
struct ArtifactPathQuery {
    path: String,
}

/// Delete a file or folder in the artifacts dir, by relative path.
async fn delete_artifact(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ArtifactPathQuery>,
) -> ApiResult {
    reject_if_moving(&state)?;
    if local::files::is_project_brief_path(&q.path) {
        return Err(bad_request(
            "PROJECT.md is part of the project and cannot be deleted",
        ));
    }
    blocking_api(move || {
        let store = Store::open()?;
        let project = store
            .get_local_project(&id)?
            .ok_or_else(|| not_found("project"))?;
        local::files::delete_entry(&project, &q.path)?;
        Ok(Json(json!({ "ok": true })))
    })
    .await
}

#[derive(Deserialize)]
struct ProjectBriefWriteReq {
    content: String,
}

async fn write_project_brief(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<ProjectBriefWriteReq>,
) -> ApiResult {
    reject_if_moving(&state)?;
    if req.content.len() > local::files::MAX_PROJECT_BRIEF_BYTES {
        return Err(bad_request(
            "PROJECT.md is too large; keep the project brief under 256 KiB",
        ));
    }
    blocking_api(move || {
        let store = Store::open()?;
        let project = store
            .get_local_project(&id)?
            .ok_or_else(|| not_found("project"))?;
        local::files::write_project_brief(&project, &req.content)?;
        Ok(Json(json!({
            "ok": true,
            "bytesWritten": req.content.len(),
        })))
    })
    .await
}

/// Raw artifact bytes, by directory-relative path. `no-cache`: the same path can
/// be rewritten in place on disk.
async fn serve_artifact(
    Path(id): Path<String>,
    Query(q): Query<ArtifactPathQuery>,
    method: Method,
    headers: HeaderMap,
) -> std::result::Result<Response, ApiError> {
    let display_path = q.path.clone();
    let file = tokio::task::spawn_blocking(move || {
        let store = Store::open()?;
        let project = store
            .get_local_project(&id)?
            .ok_or_else(|| not_found("project"))?;
        let path = local::files::file_path(&project, &q.path).map_err(|_| not_found("file"))?;
        let file = std::fs::File::open(&path).map_err(|_| not_found("file"))?;
        let metadata = file
            .metadata()
            .map_err(|e| ApiError::from(anyhow!("stat failed: {e}")))?;
        if !metadata.is_file() {
            return Err(not_found("file"));
        }
        Ok(file)
    })
    .await
    .map_err(|e| ApiError::from(anyhow!("file task failed: {e}")))??;
    let presentation = local::files::presentation_for_path(&display_path);
    crate::commands::file_serve::disk_response(
        &display_path,
        file,
        presentation,
        &method,
        &headers,
        "no-cache",
    )
    .await
    .map_err(ApiError::from)
}

// --- HF token settings ------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HfSettings {
    configured: bool,
    source: Option<&'static str>,
    masked_token: Option<String>,
    valid: bool,
    username: Option<String>,
    jobs_write: Option<bool>,
}

/// Never the full token: first 3 chars + ellipsis + last 4.
fn mask_token(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() < 8 {
        return "…".to_string();
    }
    format!(
        "{}…{}",
        chars[..3].iter().collect::<String>(),
        chars[chars.len() - 4..].iter().collect::<String>()
    )
}

/// Re-resolve the token and check it against whoami-v2. Uncached — cheap, and
/// the UI calls it rarely.
async fn hf_token_status() -> HfSettings {
    use crate::jobs::huggingface::{self, TokenSource};
    let Ok((token, source)) = huggingface::resolve_token_with_source() else {
        return HfSettings {
            configured: false,
            source: None,
            masked_token: None,
            valid: false,
            username: None,
            jobs_write: None,
        };
    };
    let source = match source {
        TokenSource::Env => "env",
        TokenSource::OpenresearchEnv => "openresearchEnv",
        TokenSource::HfCache => "hfCache",
    };
    let details = huggingface::whoami_details(&token).await.ok();
    HfSettings {
        configured: true,
        source: Some(source),
        masked_token: Some(mask_token(&token)),
        valid: details.is_some(),
        username: details.as_ref().map(|d| d.name.clone()),
        jobs_write: details.and_then(|d| d.jobs_write),
    }
}

async fn hf_settings() -> Json<Value> {
    Json(json!(hf_token_status().await))
}

#[derive(Deserialize)]
struct SetHfTokenReq {
    token: String,
}

async fn set_hf_token(Json(req): Json<SetHfTokenReq>) -> ApiResult {
    let token = req.token.trim().to_string();
    if token.is_empty() {
        return Err(bad_request("token is required"));
    }
    crate::jobs::huggingface::whoami_details(&token)
        .await
        .map_err(bad_request)?;
    tokio::task::spawn_blocking(move || crate::config::write_synced_env_var("HF_TOKEN", &token))
        .await
        .map_err(|e| anyhow!("env write task failed: {e}"))??;
    // Freshly re-resolved: if HF_TOKEN is set in this process env, env still
    // wins over the file — source says "env" and the UI explains it.
    Ok(Json(json!(hf_token_status().await)))
}

/// Keep a long-running dashboard current. `main`'s invocation-time check only
/// fires once, and `orx up` (and the macOS app it backs) can stay up for days —
/// long enough to drift several releases behind without this.
fn spawn_update_checker() {
    tokio::spawn(async {
        loop {
            updates::periodic_update_pass().await;
            tokio::time::sleep(updates::PERIODIC_CHECK_INTERVAL).await;
        }
    });
}

/// Startup summary of detected coding agents. Never blocks.
fn spawn_agent_preflight() {
    tokio::spawn(async {
        let harnesses = local::harness::detect_harnesses().await;
        let line: Vec<String> = harnesses
            .iter()
            .map(|h| {
                if h.agent_ready {
                    match &h.account {
                        Some(acct) => format!("{} ✓ ({acct})", h.name),
                        None => format!("{} ✓", h.name),
                    }
                } else if h.installed {
                    format!("{} — not signed in", h.name)
                } else {
                    format!("{} — not installed", h.name)
                }
            })
            .collect();
        eprintln!("orx up: agents: {}", line.join(" · "));
        if !harnesses.iter().any(|h| h.agent_ready) {
            eprintln!(
                "orx up: warning: no coding agent detected — install Claude Code, Codex or OpenCode and sign in to at least one of them."
            );
        }
    });
}

/// A signed-out harness is the only state that needs polling. Normal turns and
/// healthy idle sessions do no auth work; this loop merely notices a login the
/// user completed separately and wakes the UI immediately.
fn spawn_claude_auth_monitor(
    chat: Arc<ChatHost>,
    claude: Arc<local::claude::ClaudeHost>,
    harnesses: Arc<tokio::sync::Mutex<Option<(std::time::Instant, Value)>>>,
) {
    tokio::spawn(async move {
        let mut delay = Duration::from_secs(5);
        let mut observed_generation = claude.auth_snapshot().generation;
        loop {
            tokio::time::sleep(delay).await;
            let before = claude.auth_snapshot();
            if before.generation != observed_generation {
                observed_generation = before.generation;
                *harnesses.lock().await = None;
                if claude.claim_auth_announcement(before.generation) {
                    chat.emit_event(
                        "harness.auth",
                        json!({ "harness": "claude-code", "authState": before.state }),
                    );
                }
            }
            if before.runtime_rejected
                || matches!(
                    before.state,
                    local::harness::HarnessAuthState::Ready
                        | local::harness::HarnessAuthState::Unsupported
                )
            {
                delay = Duration::from_secs(5);
                continue;
            }
            let state = local::harness::claude::current_auth_state().await;
            claude.observe_auth_state(state, before.generation);
            let after = claude.auth_snapshot();
            if after.generation != observed_generation {
                observed_generation = after.generation;
                *harnesses.lock().await = None;
                if claude.claim_auth_announcement(after.generation) {
                    chat.emit_event(
                        "harness.auth",
                        json!({ "harness": "claude-code", "authState": after.state }),
                    );
                }
            }
            delay = if state == local::harness::HarnessAuthState::Unknown {
                (delay * 2).min(Duration::from_secs(60))
            } else {
                Duration::from_secs(5)
            };
        }
    });
}

// --- modal settings -----------------------------------------------------------

use crate::jobs::modal;

fn modal_settings_json(s: &modal::ModalStatus) -> Value {
    json!({
        "envProvisioned": s.env_provisioned,
        "modalImportable": s.modal_importable,
        "tokenConfigured": s.token_configured,
        "tokenSource": s.token_source,
        "ready": s.modal_importable && s.token_configured,
        "error": s.error,
    })
}

async fn modal_settings() -> Json<Value> {
    Json(modal_settings_json(&modal::detect().await))
}

/// Build the orx-managed Modal env (first run downloads the SDK, ~30–60s), then
/// report status. Idempotent — a no-op once the env exists.
async fn provision_modal() -> ApiResult {
    modal::ensure_env().await.map_err(bad_request)?;
    Ok(Json(modal_settings_json(&modal::detect().await)))
}

// --- kubernetes settings ------------------------------------------------------

use crate::jobs::kubernetes as k8s;

/// One payload powers the whole settings card: stored config plus live
/// cluster health. Contexts come from the local kubeconfig. Resource shapes
/// live in each experiment's committed manifest, not in settings.
async fn k8s_settings_json() -> Value {
    let settings = k8s::load_settings().ok().flatten();
    let configured = settings.is_some();
    let settings = settings.unwrap_or_default();
    let (contexts, current) = k8s::list_contexts().await.unwrap_or((Vec::new(), None));
    let preflight = k8s::preflight(settings.context.as_deref(), &settings.namespace).await;
    json!({
        "configured": configured,
        "contexts": contexts,
        "currentContext": current,
        "context": settings.context,
        "namespace": settings.namespace,
        "preflight": preflight,
    })
}

async fn k8s_settings() -> ApiResult {
    Ok(Json(k8s_settings_json().await))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetK8sSettingsReq {
    /// `None` leaves the field alone; `Some("")` clears it (kubectl default).
    context: Option<String>,
    namespace: Option<String>,
}

async fn set_k8s_settings(Json(req): Json<SetK8sSettingsReq>) -> ApiResult {
    let mut settings = k8s::load_settings()?.unwrap_or_default();
    if let Some(ctx) = req.context {
        settings.context = Some(ctx.trim().to_string()).filter(|c| !c.is_empty());
    }
    if let Some(ns) = req.namespace {
        let ns = ns.trim().to_string();
        settings.namespace = if ns.is_empty() {
            "default".to_string()
        } else {
            ns
        };
    }
    k8s::save_settings(&settings)?;
    Ok(Json(k8s_settings_json().await))
}

// --- env var settings -------------------------------------------------------

/// Everything in `~/.openresearch/env`, values masked. `inProcessEnv` flags
/// keys that are also set in orx up's own environment (which wins at runtime).
fn env_settings_json() -> Value {
    let vars: Vec<Value> = crate::config::list_synced_env()
        .iter()
        .map(|(key, value)| {
            json!({
                "key": key,
                "maskedValue": mask_token(value),
                "inProcessEnv": std::env::var_os(key).is_some(),
            })
        })
        .collect();
    json!({ "vars": vars })
}

async fn env_settings() -> ApiResult {
    tokio::task::spawn_blocking(|| Ok(Json(env_settings_json())))
        .await
        .map_err(|e| ApiError::from(anyhow!("env task failed: {e}")))?
}

#[derive(Deserialize)]
struct SetEnvVarReq {
    key: String,
    value: String,
}

fn valid_env_key(key: &str) -> bool {
    !key.is_empty()
        && !key.starts_with(|c: char| c.is_ascii_digit())
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

async fn set_env_var(Json(req): Json<SetEnvVarReq>) -> ApiResult {
    let key = req.key.trim().to_string();
    let value = req.value.trim().to_string();
    if !valid_env_key(&key) {
        return Err(bad_request(
            "key must be letters, digits or _, not starting with a digit",
        ));
    }
    if value.is_empty() {
        return Err(bad_request("value is required"));
    }
    tokio::task::spawn_blocking(move || {
        crate::config::write_synced_env_var(&key, &value)?;
        Ok(Json(env_settings_json()))
    })
    .await
    .map_err(|e| ApiError::from(anyhow!("env task failed: {e}")))?
}

async fn delete_env_var(Path(key): Path<String>) -> ApiResult {
    if !valid_env_key(&key) {
        return Err(bad_request("invalid key"));
    }
    tokio::task::spawn_blocking(move || {
        crate::config::remove_synced_env_var(&key)?;
        Ok(Json(env_settings_json()))
    })
    .await
    .map_err(|e| ApiError::from(anyhow!("env task failed: {e}")))?
}

// --- data directory ---------------------------------------------------------

/// Current data-dir state for the Storage settings card: where it resolves,
/// whether that's the default, the path we'd fall back to, and *why* it resolves
/// where it does (so the UI can lock the field when `$ORX_DATA_DIR` forces it).
fn data_dir_json() -> Value {
    use crate::store::DataDirSource;
    let current = crate::store::data_dir();
    let default = crate::store::default_data_dir();
    let source = crate::store::data_dir_source();
    json!({
        "current": current.to_string_lossy(),
        "defaultPath": default.to_string_lossy(),
        // "On the fallback chain" = no explicit choice (env pin or saved config).
        // Env can happen to equal the default path but is still a forced override.
        "isDefault": matches!(source, DataDirSource::Xdg | DataDirSource::Default),
        // env | config | xdg | default — env means a forced override (read-only).
        "source": source,
    })
}

async fn data_dir_settings() -> ApiResult {
    tokio::task::spawn_blocking(|| Ok(Json(data_dir_json())))
        .await
        .map_err(|e| ApiError::from(anyhow!("data-dir task failed: {e}")))?
}

#[derive(Deserialize)]
struct DataDirReq {
    path: String,
}

/// Reject a mutation when `$ORX_DATA_DIR` is forcing the path — the config value
/// would be shadowed, so honoring the request would silently do nothing.
fn ensure_not_env_forced() -> std::result::Result<(), ApiError> {
    if crate::store::data_dir_source() == crate::store::DataDirSource::Env {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "The data directory is pinned by the ORX_DATA_DIR environment \
             variable, which overrides this setting. Unset it to choose a path here."
                .into(),
        ));
    }
    Ok(())
}

/// Pre-flight a candidate path for a **move** without committing: absolute? empty
/// target? room? Returns `{ ok, error?, treeBytes, freeBytes?, sameFilesystem }`.
async fn validate_data_dir(Json(req): Json<DataDirReq>) -> ApiResult {
    use crate::local::datadir::TargetIntent;
    let path = req.path.trim().to_string();
    if path.is_empty() {
        return Err(bad_request("path is required"));
    }
    tokio::task::spawn_blocking(move || {
        match crate::local::datadir::validate_target(
            std::path::Path::new(&path),
            TargetIntent::Move,
        ) {
            Ok(report) => Ok(Json(json!({
                "ok": true,
                "treeBytes": report.tree_bytes,
                "freeBytes": report.free_bytes,
                "sameFilesystem": report.same_filesystem,
            }))),
            Err(e) => Ok(Json(json!({ "ok": false, "error": e.to_string() }))),
        }
    })
    .await
    .map_err(|e| ApiError::from(anyhow!("validate task failed: {e}")))?
}

/// Set the data dir *without moving* — for onboarding on an empty install, or
/// reconnecting to an already-populated location (a second machine, after config
/// loss). The UI routes here only when the current dir has nothing to migrate;
/// otherwise it calls `/move`. Uses `TargetIntent::Set`, which (unlike `Move`)
/// permits a populated existing dir since nothing is copied.
async fn set_data_dir(State(state): State<AppState>, Json(req): Json<DataDirReq>) -> ApiResult {
    use crate::local::datadir::TargetIntent;
    reject_if_moving(&state)?;
    ensure_not_env_forced()?;
    let path = req.path.trim().to_string();
    if path.is_empty() {
        return Err(bad_request("path is required"));
    }
    // Validate before persisting so we never store a bad path.
    let validate_path = path.clone();
    tokio::task::spawn_blocking(move || {
        crate::local::datadir::validate_target(
            std::path::Path::new(&validate_path),
            TargetIntent::Set,
        )
    })
    .await
    .map_err(|e| ApiError::from(anyhow!("validate task failed: {e}")))?
    .map_err(bad_request)?;

    tokio::task::spawn_blocking(move || crate::config::set_settings_data_dir(Some(path)))
        .await
        .map_err(|e| ApiError::from(anyhow!("settings task failed: {e}")))??;
    Ok(Json(data_dir_json()))
}

/// Relocate the data dir to `path`, streaming `datadir.move.*` progress events
/// over `/api/events`. Returns 202 immediately; the UI watches the SSE stream.
///
/// Concurrency safety: sets `data_dir_move_in_progress` *first*, then refuses
/// (409) if a run or chat turn is already active. The substantive store-mutating
/// handlers (`send`/`launch`, project/experiment/chat CRUD, file delete,
/// `set_data_dir`) check the flag on entry and back off, so once the move is
/// underway nothing new writes the store. Even the residual races don't lose
/// data: a request that passed its own flag check in the tiny window before this
/// one set the flag — or an unguarded incidental write (an `open_project`
/// timestamp touch, an `ssh_preflight` test row) — lands in the *old* dir, but
/// the cross-filesystem path never deletes it (it's returned as `oldPathLeft`),
/// so the write is preserved there; only the atomic same-filesystem rename
/// consumes the old dir, and that path has no copy window.
async fn move_data_dir(State(state): State<AppState>, Json(req): Json<DataDirReq>) -> Response {
    use crate::local::datadir::TargetIntent;
    use std::sync::atomic::Ordering;

    if let Err(e) = ensure_not_env_forced() {
        return e.into_response();
    }
    let path = req.path.trim().to_string();
    if path.is_empty() {
        return bad_request("path is required").into_response();
    }

    // Claim the move slot first (compare-exchange): only one move at a time, and
    // once claimed, new turns/launches see the flag and back off.
    if state
        .data_dir_move_in_progress
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return ApiError(
            StatusCode::CONFLICT,
            "A data-directory move is already in progress.".into(),
        )
        .into_response();
    }

    // Helper to release the slot on any early return.
    let release = |state: &AppState| {
        state
            .data_dir_move_in_progress
            .store(false, Ordering::SeqCst);
    };

    let data_dir_guard = state.data_dir_gate.clone().lock_owned().await;
    let source_data_dir = crate::store::data_dir();
    let move_token = uuid::Uuid::new_v4().to_string();
    let (move_store, move_lock) = match Store::open().and_then(|store| {
        let lock = store.acquire_data_dir_move_lock()?;
        Ok((store, lock))
    }) {
        Ok(claim) => claim,
        Err(_) => {
            release(&state);
            return ApiError(
                StatusCode::CONFLICT,
                "Another dashboard is moving the data directory.".into(),
            )
            .into_response();
        }
    };
    let move_claimed = move_store.claim_data_dir_move(&move_token).unwrap_or(false);
    if !move_claimed {
        release(&state);
        return ApiError(
            StatusCode::CONFLICT,
            "Can't move while another dashboard has an active chat turn or storage move.".into(),
        )
        .into_response();
    }

    let release_move_claim = |token: &str| {
        if let Ok(store) = Store::open() {
            let _ = store.release_data_dir_move(token);
        }
    };

    // In-flight guard: block if a chat turn or a run is active right now. (The
    // flag we just set prevents *new* ones from starting past this point.)
    let busy = state.chat.busy_sessions().await;
    let active_runs = tokio::task::spawn_blocking(active_run_count)
        .await
        .unwrap_or(0);
    let active_operations = state.project_lifecycle.operation_count();
    if !busy.is_empty() || active_runs > 0 || active_operations > 0 {
        release_move_claim(&move_token);
        release(&state);
        return ApiError(
            StatusCode::CONFLICT,
            format!(
                "Can't move while work is in progress ({} active chat turn(s), \
                 {active_runs} active run(s), {active_operations} project operation(s)). \
                 Finish or stop them, then retry.",
                busy.len()
            ),
        )
        .into_response();
    }

    // Validate before kicking off the background move.
    let vpath = path.clone();
    let validated = tokio::task::spawn_blocking(move || {
        crate::local::datadir::validate_target(std::path::Path::new(&vpath), TargetIntent::Move)
    })
    .await;
    match validated {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            release_move_claim(&move_token);
            release(&state);
            return bad_request(e).into_response();
        }
        Err(e) => {
            release_move_claim(&move_token);
            release(&state);
            return ApiError::from(anyhow!("validate task failed: {e}")).into_response();
        }
    }

    // Spawn the move on a blocking task (it does synchronous FS work); forward
    // throttled progress onto the SSE broadcast, clear the flag when done.
    let chat = state.chat.clone();
    let flag = state.data_dir_move_in_progress.clone();
    let target = std::path::PathBuf::from(path);
    tokio::spawn(async move {
        let _data_dir_guard = data_dir_guard;
        let _move_lock = move_lock;
        use crate::local::datadir::MoveProgress;
        let chat_for_progress = chat.clone();
        // Throttle: forward at most one progress event per ~120ms of copy, but
        // always emit phase edges (copied==0 or ==total) so the first/last tick
        // of every phase gets through.
        let last = std::sync::Mutex::new(0i64);
        let on_progress = move |p: MoveProgress| {
            let now = crate::store::now_ms();
            let mut guard = last.lock().unwrap();
            let is_edge = p.copied_bytes == 0 || p.copied_bytes >= p.total_bytes;
            if is_edge || now - *guard >= 120 {
                *guard = now;
                chat_for_progress.emit_event("datadir.move.progress", json!(p));
            }
        };
        let target_for_move = target.clone();
        let result = tokio::task::spawn_blocking(move || {
            crate::local::datadir::move_data_dir(target_for_move, on_progress)
        })
        .await;

        match result {
            Ok(Ok(outcome)) => {
                // Restart harness children so any that pinned the old data dir
                // (Codex hard-pins $ORX_DATA_DIR at spawn) respawn on the new one.
                chat.shutdown_harnesses().await;
                chat.emit_event("datadir.move.done", json!(outcome));
            }
            Ok(Err(e)) => chat.emit_event("datadir.move.error", json!({ "error": e.to_string() })),
            Err(e) => chat.emit_event(
                "datadir.move.error",
                json!({ "error": format!("move task panicked: {e}") }),
            ),
        }
        // A cross-filesystem copy retains the source DB, while a rename removes
        // it. Avoid reopening a removed source path and recreating it.
        if source_data_dir.exists() {
            if let Ok(store) = Store::open_at(source_data_dir) {
                let _ = store.release_data_dir_move(&move_token);
            }
        }
        if let Ok(store) = Store::open() {
            let _ = store.release_data_dir_move(&move_token);
        }
        flag.store(false, Ordering::SeqCst);
    });

    (StatusCode::ACCEPTED, Json(json!({ "started": true }))).into_response()
}

/// Count runs currently in an active state (`starting`/`running`), for the
/// data-dir move's in-flight guard. SQL-side and unbounded (see
/// `Store::count_active_runs`).
fn active_run_count() -> usize {
    Store::open()
        .and_then(|s| s.count_active_runs())
        .unwrap_or(0)
}

/// Refuse an operation that would write the store while a data-dir move is in
/// progress — the move relies on nothing new touching the old dir mid-flight.
fn reject_if_moving(state: &AppState) -> std::result::Result<(), ApiError> {
    if state
        .data_dir_move_in_progress
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "A data-directory move is in progress. Try again once it finishes.".into(),
        ));
    }
    Ok(())
}

// --- git settings -----------------------------------------------------------

fn git_out(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn git_settings_json() -> Value {
    let gh_installed = std::process::Command::new("gh")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    json!({
        "gitVersion": git_out(&["--version"]),
        "userName": git_out(&["config", "--global", "user.name"]),
        "userEmail": git_out(&["config", "--global", "user.email"]),
        "ghInstalled": gh_installed,
        "githubTokenSource": github_token_source(),
    })
}

fn project_defaults_json() -> Value {
    let token_source = github_token_source();
    json!({
        "githubForNewProjects": crate::config::github_for_new_projects(),
        "githubDefaultPromptSeen": crate::config::github_default_prompt_seen(),
        "githubAuthenticated": token_source.is_some(),
        "githubTokenSource": token_source,
    })
}

async fn project_defaults() -> ApiResult {
    Ok(Json(project_defaults_json()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetProjectDefaultsReq {
    github_for_new_projects: bool,
    #[serde(default)]
    github_default_prompt_seen: Option<bool>,
}

async fn set_project_defaults(Json(req): Json<SetProjectDefaultsReq>) -> ApiResult {
    if req.github_for_new_projects && github_token_source().is_none() {
        return Err(bad_request(
            "Connect GitHub before enabling it by default for new projects.",
        ));
    }
    crate::config::set_github_for_new_projects(req.github_for_new_projects)?;
    if let Some(seen) = req.github_default_prompt_seen {
        crate::config::set_github_default_prompt_seen(seen)?;
    }
    Ok(Json(project_defaults_json()))
}

#[derive(Deserialize)]
struct SetGitTokenReq {
    token: String,
}

async fn set_git_token(Json(req): Json<SetGitTokenReq>) -> ApiResult {
    let token = req.token.trim().to_string();
    if token.is_empty() {
        return Err(bad_request("token is required"));
    }
    let response = reqwest::Client::new()
        .get("https://api.github.com/user")
        .header("User-Agent", "orx")
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|error| bad_request(format!("Could not reach api.github.com: {error}")))?;
    if !response.status().is_success() {
        return Err(bad_request(format!(
            "GitHub rejected the token ({}).",
            response.status()
        )));
    }
    let scopes = response
        .headers()
        .get("x-oauth-scopes")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !scopes.trim().is_empty() && !scopes.split(',').any(|scope| scope.trim() == "repo") {
        return Err(bad_request(
            "Token is valid but lacks the `repo` scope needed for private repositories.",
        ));
    }
    crate::config::write_synced_env_var("GITHUB_TOKEN", &token)?;
    Ok(Json(git_settings_json()))
}

async fn delete_git_token() -> ApiResult {
    crate::config::remove_synced_env_var("GITHUB_TOKEN")?;
    Ok(Json(git_settings_json()))
}

async fn git_settings() -> ApiResult {
    tokio::task::spawn_blocking(|| Ok(Json(git_settings_json())))
        .await
        .map_err(|e| ApiError::from(anyhow!("git task failed: {e}")))?
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetGitSettingsReq {
    user_name: Option<String>,
    user_email: Option<String>,
}

async fn set_git_settings(Json(req): Json<SetGitSettingsReq>) -> ApiResult {
    let name = req.user_name.map(|s| s.trim().to_string());
    let email = req.user_email.map(|s| s.trim().to_string());
    if name.as_deref().is_none_or(str::is_empty) && email.as_deref().is_none_or(str::is_empty) {
        return Err(bad_request(
            "nothing to update: pass userName and/or userEmail",
        ));
    }
    tokio::task::spawn_blocking(move || {
        for (key, value) in [("user.name", name), ("user.email", email)] {
            if let Some(v) = value.filter(|v| !v.is_empty()) {
                let ok = std::process::Command::new("git")
                    .args(["config", "--global", key, &v])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if !ok {
                    return Err(bad_request(format!("git config --global {key} failed")));
                }
            }
        }
        Ok(Json(git_settings_json()))
    })
    .await
    .map_err(|e| ApiError::from(anyhow!("git task failed: {e}")))?
}

// --- telemetry settings -----------------------------------------------------

/// `{ enabled, reason }` — the effective analytics state after build/runtime
/// eligibility, per-run flags, and the persisted preference. `reason` is null
/// when enabled.
fn telemetry_settings_json() -> Value {
    match crate::telemetry::effective_disabled_reason() {
        None => json!({ "enabled": true, "reason": null }),
        Some(r) => json!({ "enabled": false, "reason": r.as_str() }),
    }
}

async fn telemetry_settings() -> ApiResult {
    tokio::task::spawn_blocking(|| Ok(Json(telemetry_settings_json())))
        .await
        .map_err(|e| ApiError::from(anyhow!("telemetry task failed: {e}")))?
}

#[derive(Deserialize)]
struct SetTelemetryReq {
    enabled: bool,
}

async fn set_telemetry_settings(Json(req): Json<SetTelemetryReq>) -> ApiResult {
    let enabled = req.enabled;
    tokio::task::spawn_blocking(move || {
        crate::telemetry::set_persisted_disabled(!enabled)
            .map_err(|e| ApiError::from(anyhow!("could not save telemetry setting: {e}")))?;
        Ok(Json(telemetry_settings_json()))
    })
    .await
    .map_err(|e| ApiError::from(anyhow!("telemetry task failed: {e}")))?
}

// --- updates -----------------------------------------------------------------

async fn update_status() -> ApiResult {
    tokio::task::spawn_blocking(|| Ok(Json(json!(updates::status()))))
        .await
        .map_err(|e| ApiError::from(anyhow!("update status task failed: {e}")))?
}

#[derive(Deserialize)]
struct SetAutoUpdateReq {
    enabled: bool,
}

async fn set_auto_update(Json(req): Json<SetAutoUpdateReq>) -> ApiResult {
    let enabled = req.enabled;
    tokio::task::spawn_blocking(move || {
        crate::config::set_auto_update_enabled(enabled)
            .map_err(|e| ApiError::from(anyhow!("could not save the auto-update setting: {e}")))?;
        Ok(Json(json!(updates::status())))
    })
    .await
    .map_err(|e| ApiError::from(anyhow!("auto-update task failed: {e}")))?
}

/// Apply an update now, for the user who doesn't want to wait for the next
/// periodic pass. Runs the same detached updater, so it can't race one already
/// in flight — the updater's file lock settles that.
async fn apply_update() -> ApiResult {
    updates::apply_now().await?;
    Ok(Json(json!(updates::status())))
}

#[derive(Deserialize)]
struct InstallCliReq {
    /// Replace an `orx` that is already on PATH. The card only sends this after
    /// showing the user what it would displace.
    #[serde(default)]
    force: bool,
}

/// Link the app's `orx` onto the user's PATH (Settings → Updates).
async fn install_cli(Json(req): Json<InstallCliReq>) -> ApiResult {
    let installed =
        tokio::task::spawn_blocking(move || crate::commands::install_cli::install(req.force))
            .await
            .map_err(|e| ApiError::from(anyhow!("install-cli task failed: {e}")))??;
    Ok(Json(json!({
        "link": installed.link.to_string_lossy(),
        "dir": installed.link.parent().unwrap_or(&installed.link).to_string_lossy(),
        "target": installed.target.to_string_lossy(),
        "onPath": installed.on_path,
        "alreadyCurrent": installed.already_current,
    })))
}

fn profile_settings_json() -> Value {
    json!(crate::telemetry::load_profile())
}

async fn profile_settings() -> ApiResult {
    tokio::task::spawn_blocking(|| Ok(Json(profile_settings_json())))
        .await
        .map_err(|e| ApiError::from(anyhow!("profile task failed: {e}")))?
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetProfileReq {
    #[serde(default)]
    research_areas: Vec<String>,
    #[serde(default)]
    other_area: Option<String>,
    #[serde(default)]
    background: Option<String>,
    #[serde(default)]
    papers: Vec<crate::telemetry::ProfilePaper>,
}

fn profile_settings_update(
    req: SetProfileReq,
    current: crate::telemetry::ResearchProfile,
) -> std::result::Result<crate::telemetry::ResearchProfile, ApiError> {
    if req.research_areas.is_empty() {
        Ok(crate::telemetry::ResearchProfile {
            research_areas: current.research_areas,
            other_area: current.other_area,
            background: req.background.filter(|value| !value.trim().is_empty()),
            papers: req.papers,
        })
    } else {
        normalize_research_profile(
            req.research_areas,
            req.other_area,
            req.background,
            req.papers,
        )
    }
}

async fn set_profile_settings(Json(req): Json<SetProfileReq>) -> ApiResult {
    let profile = profile_settings_update(req, crate::telemetry::load_profile())?;
    tokio::task::spawn_blocking(move || {
        crate::telemetry::set_profile(profile)
            .map_err(|e| ApiError::from(anyhow!("could not save profile: {e}")))?;
        Ok(Json(profile_settings_json()))
    })
    .await
    .map_err(|e| ApiError::from(anyhow!("profile task failed: {e}")))?
}

async fn ui_state() -> ApiResult {
    tokio::task::spawn_blocking(|| -> Result<Json<Value>> {
        Ok(Json(json!(Store::open()?.ui_state()?)))
    })
    .await
    .map_err(|error| ApiError::from(anyhow!("UI state task failed: {error}")))?
    .map_err(ApiError::from)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetUiStateReq {
    #[serde(default)]
    tour_completed: Option<bool>,
    #[serde(default)]
    preferred_agent: Option<StoredAgentSelectionReq>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredAgentSelectionReq {
    harness: String,
    model: Option<String>,
    permission_mode: Option<String>,
    reasoning_level: Option<String>,
}

async fn set_ui_state(Json(req): Json<SetUiStateReq>) -> ApiResult {
    tokio::task::spawn_blocking(move || -> Result<Json<Value>> {
        let store = Store::open()?;
        let selection = req
            .preferred_agent
            .map(|selection| {
                if !local::harness::is_chat_harness(&selection.harness) {
                    return Err(anyhow!("unknown harness: {}", selection.harness));
                }
                let nonempty = |value: Option<String>| value.filter(|item| !item.trim().is_empty());
                let permission_mode = nonempty(selection.permission_mode);
                if permission_mode.as_deref().is_some_and(|mode| {
                    local::harness::permission_mode_for(&selection.harness, mode).is_none()
                }) {
                    return Err(anyhow!("invalid permission mode for selected harness"));
                }
                Ok(StoredAgentSelection {
                    harness: selection.harness.clone(),
                    model: nonempty(selection.model),
                    permission_mode: preferred_permission_mode(&selection.harness, permission_mode),
                    reasoning_level: nonempty(selection.reasoning_level),
                })
            })
            .transpose()?;
        if let Some(completed) = req.tour_completed {
            store.set_tour_completed(completed)?;
        }
        if let Some(selection) = selection {
            store.set_preferred_agent(&selection)?;
        }
        Ok(Json(json!(store.ui_state()?)))
    })
    .await
    .map_err(|error| ApiError::from(anyhow!("UI state task failed: {error}")))?
    .map_err(bad_request)
}

/// The lit-source toggles as booleans (enabled = not in the disabled set).
fn lit_sources_json() -> Value {
    let disabled = crate::config::disabled_lit_sources();
    let enabled = |name: &str| !disabled.iter().any(|d| d == name);
    json!({
        "alphaxiv": enabled(crate::LitSource::Alphaxiv.as_str()),
        "openalex": enabled(crate::LitSource::Openalex.as_str()),
        "biorxiv": enabled(crate::LitSource::Biorxiv.as_str()),
    })
}

async fn lit_sources_settings() -> ApiResult {
    tokio::task::spawn_blocking(|| Ok(Json(lit_sources_json())))
        .await
        .map_err(|e| ApiError::from(anyhow!("lit-sources task failed: {e}")))?
}

#[derive(Deserialize)]
struct SetLitSourcesReq {
    alphaxiv: bool,
    openalex: bool,
    biorxiv: bool,
}

async fn set_lit_sources_settings(Json(req): Json<SetLitSourcesReq>) -> ApiResult {
    tokio::task::spawn_blocking(move || {
        let mut disabled = Vec::new();
        for (enabled, source) in [
            (req.alphaxiv, crate::LitSource::Alphaxiv),
            (req.openalex, crate::LitSource::Openalex),
            (req.biorxiv, crate::LitSource::Biorxiv),
        ] {
            if !enabled {
                disabled.push(source.as_str().to_string());
            }
        }
        crate::telemetry::set_disabled_lit_sources(disabled)
            .map_err(|e| ApiError::from(anyhow!("could not save literature sources: {e}")))?;
        Ok(Json(lit_sources_json()))
    })
    .await
    .map_err(|e| ApiError::from(anyhow!("lit-sources task failed: {e}")))?
}

/// Record the analytics choice once when the user leaves onboarding. In an
/// eligible official build this ignores the persisted preference so opt-outs
/// are counted; development and runtime-disabled builds stay inert.
async fn record_telemetry_consent(Json(req): Json<SetTelemetryReq>) -> ApiResult {
    crate::telemetry::record_consent(req.enabled).await;
    crate::telemetry::capture_onboarding_completed();
    Ok(Json(json!({ "ok": true })))
}

// --- ssh hosts ----------------------------------------------------------------

/// Concrete Host entries from `~/.ssh/config` (wildcard patterns skipped) —
/// read-only groundwork for an SSH compute backend. No keys are read.
fn list_ssh_hosts() -> Vec<Value> {
    let Some(path) = dirs::home_dir().map(|h| h.join(".ssh").join("config")) else {
        return Vec::new();
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut hosts: Vec<Value> = Vec::new();
    // Indices into `hosts` for the Host block currently being filled.
    let mut current: Vec<usize> = Vec::new();
    for line in raw.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = match line.split_once([' ', '\t', '=']) {
            Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim().trim_matches('"')),
            None => continue,
        };
        if key == "host" {
            current = value
                .split_whitespace()
                .filter(|name| !name.contains(['*', '?', '!']))
                .map(|name| {
                    hosts.push(json!({ "host": name }));
                    hosts.len() - 1
                })
                .collect();
            continue;
        }
        let field = match key.as_str() {
            "hostname" => "hostname",
            "user" => "user",
            "port" => "port",
            "identityfile" => "identityFile",
            _ => continue,
        };
        for &i in &current {
            // First value wins, like ssh itself.
            if hosts[i].get(field).is_none() {
                hosts[i][field] = json!(value);
            }
        }
    }
    hosts
}

async fn ssh_settings() -> ApiResult {
    tokio::task::spawn_blocking(|| {
        let mut hosts = list_ssh_hosts();
        // Best-effort, like the preflight write: a store hiccup shouldn't take
        // out the host listing — hosts just render as never tested.
        let tests: HashMap<String, SshHostTest> = Store::open()
            .and_then(|s| s.list_ssh_host_tests())
            .unwrap_or_else(|e| {
                eprintln!("orx up: could not load ssh test history: {e}");
                Vec::new()
            })
            .into_iter()
            .map(|t| (t.host.clone(), t))
            .collect();
        for h in &mut hosts {
            let Some(t) = h
                .get("host")
                .and_then(Value::as_str)
                .and_then(|a| tests.get(a))
            else {
                continue;
            };
            h["lastTest"] = json!(t);
        }
        Ok(Json(json!({ "hosts": hosts })))
    })
    .await
    .map_err(|e| ApiError::from(anyhow!("ssh task failed: {e}")))?
}

#[derive(Deserialize)]
struct SshPreflightReq {
    host: String,
}

/// Live check for one host: can we reach it and run bash/tar snapshots?
async fn ssh_preflight(Json(req): Json<SshPreflightReq>) -> ApiResult {
    let host = req.host.trim().to_string();
    if host.is_empty() {
        return Err(bad_request("host is required"));
    }
    let p = crate::jobs::ssh::preflight(&crate::jobs::ssh::SshTarget::alias(&host)).await;
    let test = SshHostTest {
        host,
        reachable: p.reachable,
        tools_found: p.tools_found,
        error: p.error,
        tested_at: now_ms(),
    };
    // Best-effort persistence — the UI shows "last tested" across restarts,
    // but a store hiccup shouldn't hide a test result that already ran.
    let record = test.clone();
    if let Err(e) =
        tokio::task::spawn_blocking(move || Store::open()?.upsert_ssh_host_test(&record))
            .await
            .map_err(|e| anyhow!("ssh task failed: {e}"))
            .and_then(|r| r)
    {
        eprintln!("orx up: could not record ssh test for {}: {e}", test.host);
    }
    Ok(Json(json!(test)))
}

// --- slurm --------------------------------------------------------------------

use crate::jobs::slurm;

/// One payload powers the whole settings card: stored cluster defaults plus
/// the ssh hosts to pick a login node from (same `~/.ssh/config` source as
/// the ssh backend — a Slurm login node is just an ssh host).
fn slurm_settings_json() -> Value {
    let settings = slurm::load_settings().ok().flatten().unwrap_or_default();
    json!({
        "host": settings.host,
        "partition": settings.partition,
        "account": settings.account,
        "timeLimit": settings.time_limit,
        "hosts": list_ssh_hosts(),
    })
}

async fn slurm_settings() -> ApiResult {
    tokio::task::spawn_blocking(|| Ok(Json(slurm_settings_json())))
        .await
        .map_err(|e| ApiError::from(anyhow!("slurm task failed: {e}")))?
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetSlurmSettingsReq {
    /// `None` leaves the field alone; `Some("")` clears it (cluster default).
    host: Option<String>,
    partition: Option<String>,
    account: Option<String>,
    time_limit: Option<String>,
}

async fn set_slurm_settings(Json(req): Json<SetSlurmSettingsReq>) -> ApiResult {
    // One spawn_blocking around the whole load→mutate→save→respond body
    // (settings + ~/.ssh/config are sync fs I/O), like the git handlers.
    tokio::task::spawn_blocking(move || {
        let mut settings = slurm::load_settings()?.unwrap_or_default();
        let norm = |v: String| Some(v.trim().to_string()).filter(|s| !s.is_empty());
        if let Some(h) = req.host {
            settings.host = norm(h);
        }
        if let Some(p) = req.partition {
            settings.partition = norm(p);
        }
        if let Some(a) = req.account {
            settings.account = norm(a);
        }
        if let Some(t) = req.time_limit {
            // Reject a default that would fail every later launch.
            let t = norm(t);
            if let Some(t) = &t {
                crate::jobs::huggingface::parse_timeout(t).map_err(bad_request)?;
            }
            settings.time_limit = t;
        }
        slurm::save_settings(&settings)?;
        Ok(Json(slurm_settings_json()))
    })
    .await
    .map_err(|e| ApiError::from(anyhow!("slurm task failed: {e}")))?
}

#[derive(Deserialize)]
struct SlurmPreflightReq {
    host: String,
}

/// Live check for one login node: reachable, Slurm CLI + snapshot tools, and
/// which partitions exist (feeds the partition picker).
async fn slurm_preflight(Json(req): Json<SlurmPreflightReq>) -> ApiResult {
    let host = req.host.trim().to_string();
    if host.is_empty() {
        return Err(bad_request("host is required"));
    }
    let p = slurm::preflight(&host).await;
    Ok(Json(json!({
        "reachable": p.reachable,
        "slurmFound": p.slurm_found,
        "toolsFound": p.tools_found,
        "partitions": p.partitions,
        "error": p.error,
    })))
}

// --- ray --------------------------------------------------------------------

use crate::jobs::ray;

fn ray_settings_json() -> Value {
    let settings = ray::load_settings().ok().flatten().unwrap_or_default();
    let (resolved, source) = ray::resolve_address_with_source();
    let source_label = match source {
        ray::AddressSource::Settings => "settings",
        ray::AddressSource::AstroaiEnv => "ASTROAI_RAY_JOBS_ADDRESS",
        ray::AddressSource::RayEnv => "RAY_DASHBOARD_URL",
        ray::AddressSource::Default => "default",
    };
    json!({
        "address": settings.address,
        "resolvedAddress": resolved,
        "source": source_label,
    })
}

async fn ray_settings() -> ApiResult {
    tokio::task::spawn_blocking(|| Ok(Json(ray_settings_json())))
        .await
        .map_err(|e| ApiError::from(anyhow!("ray task failed: {e}")))?
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetRaySettingsReq {
    /// `None` leaves alone; `Some("")` clears (fall back to env / default).
    address: Option<String>,
}

async fn set_ray_settings(Json(req): Json<SetRaySettingsReq>) -> ApiResult {
    tokio::task::spawn_blocking(move || {
        let mut settings = ray::load_settings()?.unwrap_or_default();
        if let Some(a) = req.address {
            let a = Some(a.trim().to_string()).filter(|s| !s.is_empty());
            if let Some(a) = &a {
                // Reject a default that would fail every later launch.
                let url = reqwest::Url::parse(a)
                    .map_err(|e| bad_request(anyhow!("Invalid Jobs URL {a:?}: {e}")))?;
                if !matches!(url.scheme(), "http" | "https") {
                    return Err(bad_request(anyhow!(
                        "The Jobs URL must be http(s), e.g. http://127.0.0.1:8265 (got {a:?})."
                    )));
                }
            }
            settings.address = a;
        }
        ray::save_settings(&settings)?;
        Ok(Json(ray_settings_json()))
    })
    .await
    .map_err(|e| ApiError::from(anyhow!("ray task failed: {e}")))?
}

#[derive(Deserialize)]
struct RayPreflightReq {
    address: Option<String>,
}

/// Live check for a Ray Jobs / Dashboard endpoint.
async fn ray_preflight(Json(req): Json<RayPreflightReq>) -> ApiResult {
    let address = ray::resolve_address(req.address.as_deref());
    match ray::preflight(&address).await {
        Ok(ray_version) => Ok(Json(json!({
            "reachable": true,
            "address": address,
            "rayVersion": ray_version,
            "error": null,
        }))),
        Err(e) => Ok(Json(json!({
            "reachable": false,
            "address": address,
            "rayVersion": null,
            "error": e.to_string(),
        }))),
    }
}

// --- compute targets (unified settings list + default) --------------------------

/// The whole payload for the Compute tab's collapsed list, in one round trip.
/// CHEAP probes only — env vars and file reads, never a network call, kubectl,
/// or the modal python import. `configured` means "worth trying", not
/// "healthy"; deep health stays in each backend's own settings endpoint,
/// fetched when a row is expanded.
/// Whether a box would accept this machine. Three-valued on purpose: the check
/// needs the api, so "we couldn't ask" is a different answer from "no" and the
/// badge shouldn't have to pick one of the two. Each arm carries what the row
/// needs to say — guessing a key path would send the user to a file that may
/// not exist.
#[derive(Clone, PartialEq)]
enum SshReadiness {
    Ready,
    /// The `.pub` on this machine worth registering, if there is one.
    NoUsableKey {
        pub_path: Option<String>,
    },
    Unverified {
        reason: String,
    },
}

async fn openresearch_ssh_readiness() -> SshReadiness {
    use crate::local::ssh_identity::{preferred_local, tilde, KeyStatus};
    let Ok(Some(creds)) = crate::config::load_credentials().await else {
        // Signed-out is reported by the row's own `or_logged_in`.
        return SshReadiness::NoUsableKey { pub_path: None };
    };
    let named = |local: &[crate::local::ssh_identity::LocalKey]| SshReadiness::NoUsableKey {
        pub_path: preferred_local(local)
            .and_then(|k| k.path.as_deref())
            .map(tilde),
    };
    match crate::local::ssh_identity::check(&creds).await {
        KeyStatus::Matched => SshReadiness::Ready,
        KeyStatus::NoLocalMatch { local, .. } | KeyStatus::NoneRegistered { local } => {
            named(&local)
        }
        KeyStatus::Unknown { reason } => SshReadiness::Unverified { reason },
    }
}

/// The openresearch row's one-line status. Every branch that tells the user to
/// run something names a path we actually found — never a guessed one.
fn openresearch_summary(logged_in: bool, ssh: &SshReadiness) -> String {
    if !logged_in {
        return "Not signed in — run orx login".to_string();
    }
    match ssh {
        SshReadiness::Ready => "Signed in — ephemeral boxes billed to your org".to_string(),
        SshReadiness::NoUsableKey {
            pub_path: Some(path),
        } => format!("No usable SSH key — run orx ssh-key add {path}"),
        SshReadiness::NoUsableKey { pub_path: None } => {
            "No SSH key on this computer — run ssh-keygen -t ed25519, then orx ssh-key add"
                .to_string()
        }
        SshReadiness::Unverified { reason } => {
            format!("Signed in — couldn't check your SSH key ({reason})")
        }
    }
}

fn compute_settings_json(ssh: SshReadiness) -> Value {
    let default = crate::config::compute_default();
    let (default_backend, default_flavor) = match &default {
        Some((b, f)) => (Some(b.as_str()), f.as_deref()),
        None => (None, None),
    };

    let hf = crate::jobs::huggingface::resolve_token_with_source().ok();
    let modal_source = crate::jobs::modal::token_source();
    let k8s_settings = k8s::load_settings().ok().flatten();
    let ssh_hosts = list_ssh_hosts().len();
    let slurm_settings = crate::jobs::slurm::load_settings().ok().flatten();
    let slurm_host = slurm_settings.as_ref().and_then(|s| s.host.clone());
    let (ray_resolved, ray_source) = crate::jobs::ray::resolve_address_with_source();
    let ray_configured = !matches!(ray_source, crate::jobs::ray::AddressSource::Default);
    let ray_source_label = match ray_source {
        crate::jobs::ray::AddressSource::Settings => "Saved address",
        crate::jobs::ray::AddressSource::AstroaiEnv => "ASTROAI_RAY_JOBS_ADDRESS",
        crate::jobs::ray::AddressSource::RayEnv => "RAY_DASHBOARD_URL",
        crate::jobs::ray::AddressSource::Default => "Default localhost:8265",
    };
    // Presence of the credentials file only — whether the token still works is
    // the expanded row's (network) question.
    let or_logged_in = crate::config::credentials_present();

    // Same spellings as the expanded rows' SOURCE_LABELS/MODAL_TOKEN_LABELS
    // in the UI — the collapsed head stays visible above the open row, so the
    // same fact must not read two different ways.
    let source_label = |s: &crate::jobs::huggingface::TokenSource| match s {
        crate::jobs::huggingface::TokenSource::Env => "HF_TOKEN env var",
        crate::jobs::huggingface::TokenSource::OpenresearchEnv => "Token from ~/.openresearch/env",
        crate::jobs::huggingface::TokenSource::HfCache => "Token from ~/.cache/huggingface/token",
    };
    let mut targets = json!([
        {
            "id": "local",
            "configured": true,
            "summary": "Runs as a detached process on this machine",
        },
        {
            "id": "hf",
            "configured": hf.is_some(),
            "summary": hf.as_ref().map_or_else(
                || "No token".to_string(),
                |(_, s)| source_label(s).to_string(),
            ),
        },
        {
            "id": "modal",
            "configured": modal_source.is_some(),
            "summary": match modal_source {
                Some("env") => "MODAL_TOKEN_ID env var",
                Some("syncedEnv") => "Token from ~/.openresearch/env",
                Some("modalToml") => "Token from ~/.modal.toml",
                _ => "No token",
            },
        },
        {
            "id": "k8s",
            "configured": k8s_settings.is_some(),
            "summary": k8s_settings.as_ref().map_or_else(
                || "No context selected".to_string(),
                |s| format!(
                    "Context {} / namespace {}",
                    s.context.as_deref().unwrap_or("(kubectl default)"),
                    s.namespace,
                ),
            ),
        },
        {
            "id": "ssh",
            "configured": ssh_hosts > 0,
            "summary": match ssh_hosts {
                0 => "No hosts in ~/.ssh/config".to_string(),
                1 => "1 host in ~/.ssh/config".to_string(),
                n => format!("{n} hosts in ~/.ssh/config"),
            },
        },
        {
            "id": "slurm",
            "configured": slurm_host.is_some(),
            "summary": slurm_host.as_ref().map_or_else(
                || "No login node configured".to_string(),
                |h| format!("Login node {h}"),
            ),
        },
        {
            "id": "ray",
            "configured": ray_configured,
            "summary": if ray_configured {
                format!("{ray_source_label} ({ray_resolved})")
            } else {
                ray_source_label.to_string()
            },
        },
        {
            "id": "openresearch",
            // Signed in alone would be a green light on a backend that can't
            // connect — the box authorizes your *registered* keys, so one of
            // them has to be on this machine too.
            "configured": or_logged_in && ssh == SshReadiness::Ready,
            "unverified": or_logged_in && matches!(ssh, SshReadiness::Unverified { .. }),
            "summary": openresearch_summary(or_logged_in, &ssh),
        },
    ]);
    if let Some(targets) = targets.as_array_mut() {
        for target in targets {
            if let Some(target) = target.as_object_mut() {
                target.insert("enabled".to_string(), Value::Bool(true));
                target.insert("disabledReason".to_string(), Value::Null);
            }
        }
    }
    json!({
        "defaultBackend": default_backend.unwrap_or("local"),
        "defaultFlavor": default_flavor,
        "configuredDefaultBackend": default_backend,
        "configuredDefaultFlavor": default_flavor,
        "targets": targets,
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ComputeSettingsQuery {
    project_id: Option<String>,
}

async fn compute_settings(Query(query): Query<ComputeSettingsQuery>) -> ApiResult {
    let ssh = openresearch_ssh_readiness().await;
    let _project_id = query.project_id;
    // fs/env probes only, but keep them off the async runtime anyway.
    let payload =
        tokio::task::spawn_blocking(move || -> Result<Value> { Ok(compute_settings_json(ssh)) })
            .await
            .map_err(|e| ApiError::from(anyhow!("compute settings task failed: {e}")))??;
    Ok(Json(payload))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetComputeDefaultReq {
    /// `None`/absent clears the default (and its flavor with it).
    backend: Option<String>,
    flavor: Option<String>,
    project_id: Option<String>,
}

/// Persist the default compute target. An *unconfigured* backend is allowed
/// (config state fluctuates outside orx; the UI warns instead) — only unknown
/// backends and meaningless flavors are rejected.
async fn set_compute_default(Json(req): Json<SetComputeDefaultReq>) -> ApiResult {
    let _project_id = req.project_id;
    let backend = req
        .backend
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty());
    let flavor = req
        .flavor
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty());
    if let Some(b) = &backend {
        local::validate_compute_default(b, flavor.as_deref()).map_err(bad_request)?;
    }
    // Picking openresearch as the default is the moment to answer "will this
    // actually work?", so the row that comes back is honest about the SSH key.
    let ssh = openresearch_ssh_readiness().await;
    // Validation already ran above, so a failure in here is a server-side
    // fault (io error, corrupt settings.json refusal) — surface it as 500 via
    // the plain ApiError conversion, not as a 400 blaming the request.
    let payload = tokio::task::spawn_blocking(move || -> Result<Value> {
        crate::config::set_compute_default(backend, flavor)?;
        Ok(compute_settings_json(ssh))
    })
    .await
    .map_err(|e| ApiError::from(anyhow!("compute default task failed: {e}")))??;
    Ok(Json(payload))
}

/// The "This machine" row's expanded detail: detected hardware. Subprocess
/// probes (hostname, sysctl, nvidia-smi) — blocking, so spawned.
async fn local_machine_settings() -> ApiResult {
    let hw = tokio::task::spawn_blocking(crate::jobs::localbox::hardware_info)
        .await
        .map_err(|e| ApiError::from(anyhow!("hardware probe task failed: {e}")))?;
    Ok(Json(json!(hw)))
}

/// The OpenResearch row's expanded detail. Network calls are fine here (the
/// row is open) but each is individually best-effort — an offline machine
/// still renders "signed in, status unknown" instead of an error page.
async fn openresearch_settings() -> ApiResult {
    let Some(creds) = crate::config::load_credentials().await? else {
        return Ok(Json(json!({
            "loggedIn": false,
            "apiUrl": null,
            "orgs": [],
            "sshKeyStatus": "unknown",
            "error": null,
        })));
    };
    let mut error: Option<String> = None;
    let orgs = match crate::client::list_orgs(&creds).await {
        Ok(o) => o.orgs.into_iter().map(|o| o.name).collect::<Vec<_>>(),
        Err(e) => {
            error = Some(e.to_string());
            Vec::new()
        }
    };
    // "Registered" alone is a misleading green: a key registered from another
    // laptop leaves this machine unable to reach any box. Report whether the
    // private half is actually here.
    use crate::local::ssh_identity::{preferred_local, tilde, KeyStatus};
    // Hand back the .pub we actually found so the note can name a real file
    // rather than guessing at ~/.ssh/id_ed25519.pub.
    let mut ssh_key_path: Option<String> = None;
    let mut note_key = |local: &[crate::local::ssh_identity::LocalKey]| {
        ssh_key_path = preferred_local(local)
            .and_then(|k| k.path.as_deref())
            .map(tilde);
    };
    let ssh_key_status = match crate::local::ssh_identity::check(&creds).await {
        KeyStatus::Matched => "matched",
        KeyStatus::NoLocalMatch { local, .. } => {
            note_key(&local);
            "no_local_match"
        }
        KeyStatus::NoneRegistered { local } => {
            note_key(&local);
            "none_registered"
        }
        KeyStatus::Unknown { reason } => {
            error.get_or_insert(reason);
            "unknown"
        }
    };
    Ok(Json(json!({
        "loggedIn": true,
        "apiUrl": creds.api_url,
        "orgs": orgs,
        "sshKeyStatus": ssh_key_status,
        "sshKeyPath": ssh_key_path,
        "error": error,
    })))
}

// --- harnesses ---------------------------------------------------------------

const HARNESS_CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Deserialize)]
struct HarnessQuery {
    refresh: Option<u8>,
    retry: Option<u8>,
}

fn overlay_claude_auth(payload: &mut Value, snapshot: local::claude::AuthSnapshot) {
    let Some(harnesses) = payload.get_mut("harnesses").and_then(Value::as_array_mut) else {
        return;
    };
    let Some(claude) = harnesses
        .iter_mut()
        .find(|h| h.get("id").and_then(Value::as_str) == Some("claude-code"))
    else {
        return;
    };
    claude["authState"] = json!(snapshot.state);
    if snapshot.state == local::harness::HarnessAuthState::Ready {
        return;
    }
    claude["authenticated"] = json!(false);
    claude["agentReady"] = json!(false);
    claude["models"] = json!([]);
    if let Some(object) = claude.as_object_mut() {
        object.remove("authMethod");
        object.remove("account");
        object.remove("org");
        object.remove("plan");
    }
    claude["agentNote"] = json!(if snapshot.runtime_rejected {
        local::harness::claude::auth_recovery_note()
    } else {
        match snapshot.state {
            local::harness::HarnessAuthState::NeedsLogin => {
                "Sign in with `claude auth login`, then re-check this harness."
            }
            local::harness::HarnessAuthState::Unknown => {
                "Open a terminal and run `claude auth status`, then re-check this harness."
            }
            local::harness::HarnessAuthState::Unsupported => {
                "Update Claude Code to 2.1.211 or newer, then re-check this harness."
            }
            local::harness::HarnessAuthState::Ready => unreachable!(),
        }
    });
}

fn payload_has_ready_claude(payload: &Value) -> bool {
    payload
        .get("harnesses")
        .and_then(Value::as_array)
        .and_then(|harnesses| {
            harnesses
                .iter()
                .find(|h| h.get("id").and_then(Value::as_str) == Some("claude-code"))
        })
        .and_then(|claude| claude.get("agentReady"))
        .and_then(Value::as_bool)
        == Some(true)
}

fn ready_claude_entry(payload: &Value) -> Option<Value> {
    payload
        .get("harnesses")?
        .as_array()?
        .iter()
        .find(|h| {
            h.get("id").and_then(Value::as_str) == Some("claude-code")
                && h.get("agentReady").and_then(Value::as_bool) == Some(true)
        })
        .cloned()
}

fn replace_claude_entry(payload: &mut Value, replacement: Value) {
    let Some(harnesses) = payload.get_mut("harnesses").and_then(Value::as_array_mut) else {
        return;
    };
    if let Some(claude) = harnesses
        .iter_mut()
        .find(|h| h.get("id").and_then(Value::as_str) == Some("claude-code"))
    {
        *claude = replacement;
    }
}

async fn list_harnesses(
    State(state): State<AppState>,
    Query(q): Query<HarnessQuery>,
) -> Json<Value> {
    let mut cache = state.harnesses.lock().await;
    if q.retry == Some(1) && state.claude.clear_runtime_rejection() {
        *cache = None;
    }
    let prior_ready_claude = cache
        .as_ref()
        .map(|(_, payload)| payload)
        .and_then(ready_claude_entry);
    if q.refresh != Some(1) {
        if let Some((at, payload)) = cache.as_ref() {
            if at.elapsed() < HARNESS_CACHE_TTL {
                let snapshot = state.claude.auth_snapshot();
                if snapshot.state != local::harness::HarnessAuthState::Ready
                    || payload_has_ready_claude(payload)
                {
                    let mut payload = payload.clone();
                    overlay_claude_auth(&mut payload, snapshot);
                    return Json(payload);
                }
            }
        }
    }
    let probe_generation = state.claude.auth_snapshot().generation;
    let harnesses = local::harness::detect_harnesses().await;
    if let Some(claude) = harnesses.iter().find(|h| h.id == "claude-code") {
        state
            .claude
            .observe_auth_state(claude.auth_state, probe_generation);
    }
    let mut payload = json!({ "harnesses": harnesses });
    let mut snapshot = state.claude.auth_snapshot();
    if snapshot.state == local::harness::HarnessAuthState::Ready
        && !payload_has_ready_claude(&payload)
    {
        if let Some(prior) = prior_ready_claude {
            replace_claude_entry(&mut payload, prior);
        } else if let Some(retry) = local::harness::detect_harness("claude-code").await {
            state
                .claude
                .observe_auth_state(retry.auth_state, snapshot.generation);
            if retry.agent_ready {
                replace_claude_entry(&mut payload, json!(retry));
            }
            snapshot = state.claude.auth_snapshot();
        }
        if snapshot.state == local::harness::HarnessAuthState::Ready
            && !payload_has_ready_claude(&payload)
        {
            state.claude.defer_auth_verification(snapshot.generation);
            snapshot = state.claude.auth_snapshot();
        }
    }
    if state.claude.claim_auth_announcement(snapshot.generation) {
        state.chat.emit_event(
            "harness.auth",
            json!({ "harness": "claude-code", "authState": snapshot.state }),
        );
    }
    overlay_claude_auth(&mut payload, snapshot);
    *cache = Some((std::time::Instant::now(), payload.clone()));
    Json(payload)
}

// --- chat --------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionsQuery {
    project_id: String,
}

async fn list_chat_sessions(
    State(state): State<AppState>,
    Query(q): Query<SessionsQuery>,
) -> ApiResult {
    let sessions = Store::open()?.list_chat_sessions_by_project(&q.project_id)?;
    let busy = state.chat.busy_sessions().await;
    let sessions: Vec<Value> = sessions
        .iter()
        .map(|s| local::chat::session_json(s, busy.contains(&s.id)))
        .collect();
    Ok(Json(json!({ "sessions": sessions })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateChatSessionReq {
    project_id: String,
    harness: String,
    model: Option<String>,
    permission_mode: Option<String>,
    #[serde(default)]
    plan_mode: bool,
    reasoning_level: Option<String>,
}

async fn create_chat_session(
    State(state): State<AppState>,
    Json(req): Json<CreateChatSessionReq>,
) -> ApiResult {
    reject_if_moving(&state)?;
    if !local::harness::is_chat_harness(&req.harness) {
        return Err(bad_request(format!("unknown harness: {}", req.harness)));
    }
    let _admission = state
        .project_lifecycle
        .admit(&req.project_id)
        .ok_or_else(|| bad_request("project deletion is in progress"))?;
    let store = Store::open()?;
    store
        .get_local_project(&req.project_id)?
        .ok_or_else(|| not_found("project"))?;
    let nonempty = |s: Option<String>| s.filter(|v| !v.trim().is_empty());
    let permission_mode = nonempty(req.permission_mode);
    if permission_mode
        .as_deref()
        .is_some_and(|mode| local::harness::permission_mode_for(&req.harness, mode).is_none())
    {
        return Err(bad_request("invalid permission mode for selected harness"));
    }
    if req.plan_mode && !local::harness::supports_command_plan(&req.harness) {
        return Err(bad_request(
            "this harness activates Plan through permissions",
        ));
    }
    let session = StoredChatSession {
        id: format!("chat_{}", uuid::Uuid::new_v4()),
        project_id: req.project_id,
        harness: req.harness,
        native_session_id: None,
        title: None,
        title_source: None,
        model: nonempty(req.model),
        permission_mode,
        plan_mode: req.plan_mode,
        plan_reset_pending: false,
        reasoning_level: nonempty(req.reasoning_level),
        archived: false,
        context_usage_json: None,
        bootstrap_context: None,
        active_leaf_id: None,
        parent_session_id: None,
        created_at: now_ms(),
        updated_at: now_ms(),
    };
    store.create_chat_session(&session)?;
    Ok(Json(
        json!({ "session": local::chat::session_json(&session, false) }),
    ))
}

async fn delete_chat_session(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    reject_if_moving(&state)?;
    state.chat.delete_session(&id).await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateChatSessionReq {
    archived: Option<bool>,
    title: Option<String>,
    plan_mode: Option<bool>,
    permission_mode: Option<String>,
}

async fn update_chat_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateChatSessionReq>,
) -> ApiResult {
    reject_if_moving(&state)?;
    let session = if let Some(title) = req.title {
        let title = title.trim();
        if title.is_empty() {
            return Err(bad_request("title cannot be empty"));
        }
        state
            .chat
            .set_title(&id, title)
            .await?
            .ok_or_else(|| not_found("chat session"))?
    } else if let Some(archived) = req.archived {
        state
            .chat
            .set_archived(&id, archived)
            .await?
            .ok_or_else(|| not_found("chat session"))?
    } else if let Some(plan_mode) = req.plan_mode {
        state
            .chat
            .set_plan_mode(&id, plan_mode)
            .await?
            .ok_or_else(|| not_found("chat session"))?
    } else if let Some(permission_mode) = req.permission_mode {
        state
            .chat
            .set_permission_mode(&id, &permission_mode)
            .await?
            .ok_or_else(|| not_found("chat session"))?
    } else {
        return Err(bad_request("nothing to update"));
    };
    let busy = state.chat.is_busy(&id).await;
    Ok(Json(
        json!({ "session": local::chat::session_json(&session, busy) }),
    ))
}

async fn chat_messages(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    // Messages first: these are two connections, so a turn writing between them
    // can only leave the leaf *ahead* of the list, which the client falls back on
    // safely — the other order hides a reply that is already in the list.
    let messages = local::chat::list_messages(&id)?;
    let session = Store::open()?
        .get_chat_session(&id)?
        .ok_or_else(|| not_found("chat session"))?;
    // The host restores its durable queue at startup; return the live snapshot
    // so dispatch progress and cancellation are reflected immediately.
    let queued = state.chat.queued_items(&id);
    Ok(Json(json!({
        "messages": messages,
        "queued": queued,
        "activeLeafId": session.active_leaf_id,
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendChatReq {
    text: String,
    client_turn_id: Option<String>,
    model: Option<String>,
    permission_mode: Option<String>,
    plan_mode: Option<bool>,
    reasoning_level: Option<String>,
    #[serde(default)]
    images: Vec<local::chat::ImageAttachment>,
    #[serde(default)]
    annotations: Vec<local::chat::TextAnnotation>,
    /// `"steer"` hands the message to a turn already running; omitted (an
    /// older client) keeps the parked-queue path.
    mode: Option<SendMode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum SendMode {
    Steer,
}

async fn send_chat_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SendChatReq>,
) -> ApiResult {
    reject_if_moving(&state)?;
    let text = req.text.trim().to_string();
    let annotations = req
        .annotations
        .into_iter()
        .filter(|annotation| !annotation.text.trim().is_empty())
        .collect::<Vec<_>>();
    if text.is_empty() && req.images.is_empty() && annotations.is_empty() {
        return Err(bad_request("text is required"));
    }
    let overrides = local::chat::TurnOverrides {
        model: req.model,
        permission_mode: req.permission_mode,
        permission_revision: None,
        plan_mode: req.plan_mode,
        plan_revision: None,
        reasoning_level: req.reasoning_level,
    };
    // The turn runs in the background; progress streams over /api/events.
    let response = if matches!(req.mode, Some(SendMode::Steer)) {
        let result = state
            .chat
            .steer_message(
                &id,
                text,
                overrides,
                req.images,
                annotations,
                req.client_turn_id,
            )
            .await
            .map_err(|error| {
                if local::chat::is_client_turn_conflict(&error) {
                    ApiError(StatusCode::CONFLICT, error.to_string())
                } else {
                    bad_request(error)
                }
            })?;
        match result {
            Some(turn) => json!({ "ok": true, "turn": turn }),
            None => json!({ "ok": true, "steered": true }),
        }
    } else {
        let result = state
            .chat
            .send_message(
                &id,
                text,
                overrides,
                req.images,
                annotations,
                req.client_turn_id,
            )
            .await
            .map_err(|error| {
                if local::chat::is_client_turn_conflict(&error) {
                    ApiError(StatusCode::CONFLICT, error.to_string())
                } else {
                    bad_request(error)
                }
            })?;
        json!({ "ok": true, "turn": result })
    };
    crate::telemetry::capture_chat_message_sent();
    Ok(Json(response))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecoverChatReq {
    action: String,
    #[serde(default, deserialize_with = "present_nullable_string")]
    model: Option<Option<String>>,
    #[serde(default, deserialize_with = "present_nullable_string")]
    permission_mode: Option<Option<String>>,
    plan_mode: Option<bool>,
    #[serde(default, deserialize_with = "present_nullable_string")]
    reasoning_level: Option<Option<String>>,
}

fn present_nullable_string<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(Some)
}

async fn recover_chat_turn(
    State(state): State<AppState>,
    Path((id, turn_id)): Path<(String, String)>,
    Json(req): Json<RecoverChatReq>,
) -> ApiResult {
    reject_if_moving(&state)?;
    let result = state
        .chat
        .recover_turn(
            &id,
            &turn_id,
            &req.action,
            local::chat::RecoveryOverrides {
                model: req.model,
                permission_mode: req.permission_mode,
                plan_mode: req.plan_mode,
                reasoning_level: req.reasoning_level,
            },
        )
        .await
        .map_err(|error| ApiError(StatusCode::CONFLICT, error.to_string()))?;
    Ok(Json(json!({ "ok": true, "turn": result })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ForkChatReq {
    /// The message being re-sampled: an assistant reply to retry, or a user
    /// message to re-ask with `text`.
    message_id: String,
    /// Edited prompt. Absent re-sends the original message unchanged.
    text: Option<String>,
}

async fn fork_chat_turn(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<ForkChatReq>,
) -> ApiResult {
    reject_if_moving(&state)?;
    let (kind, is_edited_resubmission) = match req.text {
        Some(text) if !text.trim().is_empty() => (local::chat::ForkKind::Edit(text), true),
        Some(_) => return Err(bad_request("text is required")),
        None => (local::chat::ForkKind::Retry, false),
    };
    // A fork re-samples under the session's current settings, so it takes no
    // overrides — the turn runs in the background and streams over /api/events.
    state
        .chat
        .fork_turn(&id, &req.message_id, kind)
        .await
        .map_err(bad_request)?;
    if is_edited_resubmission {
        crate::telemetry::capture_chat_message_sent();
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SelectBranchReq {
    /// The fork to show. Its whole branch comes with it.
    leaf_id: String,
}

async fn select_chat_branch(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SelectBranchReq>,
) -> ApiResult {
    reject_if_moving(&state)?;
    state
        .chat
        .select_branch(&id, &req.leaf_id)
        .await
        .map_err(bad_request)?;
    Ok(Json(json!({ "ok": true })))
}

/// Raw bytes of a chat attachment (image or PDF), by bare file name.
async fn chat_attachment(
    Path(name): Path<String>,
    method: Method,
    headers: HeaderMap,
) -> std::result::Result<Response, ApiError> {
    // Names are server-minted (att-<uuid>__<name>.<ext>); anything else is rejected.
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        || name.contains("..")
    {
        return Err(bad_request("invalid attachment name"));
    }
    let display_name = name.clone();
    let file = tokio::task::spawn_blocking(move || {
        let path = local::chat::attachments_dir()?.join(&name);
        let file = std::fs::File::open(&path).map_err(|_| not_found("attachment"))?;
        let metadata = file.metadata().map_err(|_| not_found("attachment"))?;
        if !metadata.is_file() {
            return Err(not_found("attachment"));
        }
        Ok(file)
    })
    .await
    .map_err(|e| ApiError::from(anyhow!("attachment task failed: {e}")))??;
    crate::commands::file_serve::disk_response(
        &display_name,
        file,
        local::files::presentation_for_path(&display_name),
        &method,
        &headers,
        "max-age=31536000, immutable",
    )
    .await
    .map_err(ApiError::from)
}

async fn interrupt_chat(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    state.chat.interrupt_by_user(&id).await?;
    Ok(Json(json!({ "ok": true })))
}

/// Cancel one message parked behind a running turn (the ✕ on a queued chip).
async fn cancel_queued_chat(
    State(state): State<AppState>,
    Path((id, item_id)): Path<(String, String)>,
) -> ApiResult {
    let removed = state.chat.cancel_queued(&id, &item_id)?;
    Ok(Json(json!({ "ok": true, "removed": removed })))
}

/// Retry one queued message after its safe delivery budget was exhausted.
async fn retry_queued_chat(
    State(state): State<AppState>,
    Path((id, item_id)): Path<(String, String)>,
) -> ApiResult {
    reject_if_moving(&state)?;
    let retried = state.chat.retry_queued(&id, &item_id)?;
    if !retried {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "queued message is no longer available for retry".into(),
        ));
    }
    Ok(Json(json!({ "ok": true, "retried": retried })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RespondReq {
    prompt_id: String,
    #[serde(default = "default_true")]
    approve: bool,
    #[serde(default)]
    resume_mode: Option<String>,
    #[serde(default)]
    answers: Vec<String>,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    annotations: Vec<local::chat::TextAnnotation>,
}

fn default_true() -> bool {
    true
}

/// Answer an interactive prompt (plan / permission / question) on a session.
async fn respond_chat(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<RespondReq>,
) -> ApiResult {
    state
        .chat
        .respond(local::chat::PromptAnswer {
            session_id: id,
            prompt_id: req.prompt_id,
            approve: req.approve,
            resume_mode: req.resume_mode,
            answers: req.answers,
            note: req.note,
            annotations: req
                .annotations
                .into_iter()
                .filter(|annotation| !annotation.text.trim().is_empty())
                .collect(),
        })
        .await
        .map_err(bad_request)?;
    Ok(Json(json!({ "ok": true })))
}

/// The `orx mcp-gate` bridge relaying one blocked tool call from a plan-mode
/// claude turn. The response body is the permission decision verbatim
/// (`{"behavior":"allow",…}` / `{"behavior":"deny",…}`) — the bridge
/// stringifies it into the MCP tool result unchanged. Deliberately long-held:
/// it returns when the user answers the card (or policy/timeout decides).
async fn bridge_permission(
    State(state): State<AppState>,
    Json(req): Json<BridgePermissionReq>,
) -> ApiResult {
    let decision = state
        .chat
        .request_permission(&req.session_id, &req.token, &req.tool_name, req.tool_input)
        .await
        .map_err(bad_request)?;
    Ok(Json(serde_json::to_value(decision).map_err(bad_request)?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BridgePermissionReq {
    session_id: String,
    token: String,
    tool_name: String,
    #[serde(default)]
    tool_input: Value,
}

// --- agent ----------------------------------------------------------------

async fn agent_status(State(state): State<AppState>) -> Json<Value> {
    let agents = state.agent.status().await;
    Json(json!({ "running": !agents.is_empty(), "agents": agents }))
}

// --- /api/events SSE ------------------------------------------------------

async fn events(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = std::result::Result<Event, Infallible>>> {
    // Small buffer on purpose: run.log events can carry ~MB payloads, and a
    // stalled client must backpressure the loop, not queue hundreds of MB.
    let (tx, rx) = mpsc::channel::<Event>(16);
    tokio::spawn(event_loop(tx.clone()));
    // Chat events ride the same stream: chat.session / chat.message / chat.busy.
    let mut chat_rx = state.chat.subscribe();
    tokio::spawn(async move {
        loop {
            match chat_rx.recv().await {
                Ok((name, data)) => {
                    if tx.send(json_event(name, &data)).await.is_err() {
                        return;
                    }
                }
                // Lagged subscriber: drop missed events, keep streaming.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });
    // The guard rides the stream state, so the count drops when the response
    // body is dropped — i.e. when the tab closes or navigates away.
    let guard = DashboardClientGuard::new();
    let stream = futures::stream::unfold((rx, guard), |(mut rx, guard)| async move {
        rx.recv().await.map(|ev| (Ok(ev), (rx, guard)))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Whether a dashboard is open somewhere — macOS app mode asks on a Dock click,
/// where a live tab means "raise the browser" rather than "open the URL again".
/// Any `/api/events` consumer counts, and a connection that vanished without a
/// FIN lingers until a keep-alive write fails.
// Un-gated so CI's Linux runner still type-checks it; only macOS has a caller.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn has_live_dashboard_clients() -> bool {
    LIVE_DASHBOARD_CLIENTS.load(std::sync::atomic::Ordering::Relaxed) > 0
}

static LIVE_DASHBOARD_CLIENTS: AtomicUsize = AtomicUsize::new(0);

struct DashboardClientGuard;

impl DashboardClientGuard {
    fn new() -> Self {
        LIVE_DASHBOARD_CLIENTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self
    }
}

impl Drop for DashboardClientGuard {
    fn drop(&mut self) {
        LIVE_DASHBOARD_CLIENTS.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Diff state for one SSE subscriber.
#[derive(Default)]
struct EventCursor {
    projects: HashMap<String, i64>,
    experiments: HashMap<String, i64>,
    files: HashMap<String, u64>,
    runs: HashMap<String, (String, i64)>,
    log_offsets: HashMap<String, u64>,
    /// Last update status sent. Unlike the rest of the cursor this isn't store
    /// state — the updater is a separate process, so its progress reaches the UI
    /// through the same diff the store changes do.
    update: Option<updates::UpdateStatus>,
    update_sampled_at: Option<std::time::Instant>,
}

/// How often the event loop re-reads update status. The 500ms store cadence is
/// there for run logs; nothing about an update needs that resolution.
const UPDATE_SAMPLE_INTERVAL: Duration = Duration::from_secs(10);

/// 500ms poll loop: diff the store + log files, push named events into the
/// channel. Ends when the subscriber disconnects (send fails). Same idiom as
/// serve.rs, extended with project/experiment diffs.
async fn event_loop(tx: mpsc::Sender<Event>) {
    let mut cursor = EventCursor::default();
    let mut first = true;
    loop {
        if !first {
            // An idle store never sends, so a failed send can't be the only
            // disconnect signal — watch the receiver side too or the loop
            // (and its 2Hz store polling) leaks per closed EventSource.
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                _ = tx.closed() => return,
            }
        }
        if tx.is_closed() {
            return;
        }
        // Store hiccups (locked db) just skip a tick.
        let batch = collect_events(&mut cursor, first).unwrap_or_default();
        first = false;
        for ev in batch {
            if tx.send(ev).await.is_err() {
                return;
            }
        }
    }
}

fn json_event(name: &str, data: &Value) -> Event {
    Event::default().event(name).data(data.to_string())
}

/// One diff pass. On the first pass everything is "changed", so a fresh
/// subscriber gets a full snapshot and needs no separate baseline fetches.
fn collect_events(cursor: &mut EventCursor, first: bool) -> Result<Vec<Event>> {
    let mut out = Vec::new();

    // Sampled far below the 2Hz loop — the updater works on the scale of
    // minutes, and this reads files. `cursor.update` is committed only once the
    // batch is certain: every `?` below discards it (`event_loop` swallows the
    // error), and a cursor that had already moved would never re-emit, leaving
    // the restart banner permanently unshown for that subscriber.
    let sampled_update = cursor
        .update_sampled_at
        .is_none_or(|at| at.elapsed() >= UPDATE_SAMPLE_INTERVAL)
        .then(|| {
            cursor.update_sampled_at = Some(std::time::Instant::now());
            updates::status()
        })
        .filter(|update| cursor.update.as_ref() != Some(update));
    if let Some(update) = &sampled_update {
        out.push(json_event("update.status", &json!(update)));
    }

    let store = Store::open()?;
    // Cap log bytes per tick so one pass never materializes a huge batch —
    // remainders (whole-log replays included) stream out on later ticks.
    let mut log_budget: u64 = 2_000_000;

    for project in store.list_local_projects()? {
        if cursor.projects.get(&project.id) != Some(&project.updated_at) {
            cursor
                .projects
                .insert(project.id.clone(), project.updated_at);
            out.push(json_event(
                "project.updated",
                &json!({ "project": project_json(&project) }),
            ));
        }
        push_experiment_events(&store, &project.id, cursor, &mut out)?;
        // Artifacts appear live — anything written into the directory (by the
        // agent or the user) pings the UI to refetch the listing.
        let fp = local::files::fingerprint(&project);
        if cursor.files.get(&project.id) != Some(&fp) {
            cursor.files.insert(project.id.clone(), fp);
            out.push(json_event(
                "files.updated",
                &json!({ "projectId": project.id }),
            ));
        }
    }

    for run in store.list_runs(200)? {
        let changed = match cursor.runs.get(&run.id) {
            None => true,
            Some((status, updated)) => *status != run.status || *updated != run.updated_at,
        };
        if changed {
            cursor
                .runs
                .insert(run.id.clone(), (run.status.clone(), run.updated_at));
            out.push(json_event(
                "run.updated",
                &json!({ "run": ApiRun::from(&run) }),
            ));
        }
        if first {
            // Live runs replay their whole log through the stream (chunked per
            // tick); terminal runs start at EOF — backfill is /api/runs/{id}/log.
            let start = if is_terminal(&run.status) {
                log_size(&run.id)
            } else {
                0
            };
            cursor.log_offsets.insert(run.id.clone(), start);
        }
        // Terminal runs were seeded at EOF above, so this is a no-op for them.
        push_log_delta(&run, cursor, &mut out, &mut log_budget);
    }
    // Committed only now that the batch is certain to be returned.
    if let Some(update) = sampled_update {
        cursor.update = Some(update);
    }
    Ok(out)
}

fn push_experiment_events(
    store: &Store,
    project_id: &str,
    cursor: &mut EventCursor,
    out: &mut Vec<Event>,
) -> Result<()> {
    for exp in store.list_experiments_by_project(project_id)? {
        if cursor.experiments.get(&exp.id) != Some(&exp.updated_at) {
            cursor.experiments.insert(exp.id.clone(), exp.updated_at);
            out.push(json_event(
                "experiment.updated",
                &json!({ "experiment": exp }),
            ));
        }
    }
    Ok(())
}

fn push_log_delta(
    run: &StoredRun,
    cursor: &mut EventCursor,
    out: &mut Vec<Event>,
    budget: &mut u64,
) {
    let offset = *cursor.log_offsets.entry(run.id.clone()).or_insert(0);
    let size = log_size(&run.id);
    if size <= offset || *budget == 0 {
        return;
    }
    let chunk = read_log_from(&run.id, offset, *budget);
    *budget -= chunk.len() as u64;
    cursor
        .log_offsets
        .insert(run.id.clone(), offset + chunk.len() as u64);
    // base64: chunk boundaries are arbitrary byte positions, and exact byte
    // lengths are what lets the client dedup replays.
    out.push(json_event(
        "run.log",
        &json!({
            "runId": run.id,
            "dataBase64": base64::engine::general_purpose::STANDARD.encode(&chunk),
            "offset": offset,
        }),
    ));
}

fn is_terminal(status: &str) -> bool {
    matches!(status, "done" | "failed" | "cancelled")
}

fn log_size(run_id: &str) -> u64 {
    std::fs::metadata(log_path(run_id))
        .map(|m| m.len())
        .unwrap_or(0)
}

fn read_log_from(run_id: &str, offset: u64, max: u64) -> Vec<u8> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(log_path(run_id)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if f.seek(SeekFrom::Start(offset)).is_ok() {
        let _ = f.take(max).read_to_end(&mut out);
    }
    out
}

// --- embedded SPA ----------------------------------------------------------

/// ui/dist, embedded at release build time (debug builds read from disk).
#[derive(rust_embed::RustEmbed)]
#[folder = "ui/dist"]
struct UiDist;

const NOT_BUILT_PAGE: &str = "<!doctype html><html><head><title>orx up</title></head>\
<body style=\"font-family:system-ui;background:#111;color:#ddd;display:grid;place-items:center;height:100vh;margin:0\">\
<div><h1>UI not built</h1><p>Run <code>pnpm build</code> in <code>ui/</code>, then rebuild orx.</p>\
<p>The API is live at <code>/api/health</code>.</p></div></body></html>";

fn mime_for(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript",
        Some("css") => "text/css",
        Some("svg") => "image/svg+xml",
        Some("json") | Some("map") => "application/json",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn asset_response(path: &str, file: rust_embed::EmbeddedFile) -> Response {
    // index.html must revalidate every load or browsers heuristically cache it
    // and keep loading a stale (hashed) bundle; the hashed assets themselves
    // are immutable by name. favicon.svg is likewise served under a fixed name.
    let cache = if path == "index.html" || path == "favicon.svg" {
        "no-cache"
    } else {
        "public, max-age=31536000, immutable"
    };
    (
        [
            (header::CONTENT_TYPE, mime_for(path)),
            (header::CACHE_CONTROL, cache),
        ],
        file.data.into_owned(),
    )
        .into_response()
}

/// Every non-/api non-/opencode path: exact asset if it exists, index.html
/// otherwise (SPA client routing), friendly page when the UI isn't built.
async fn spa(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.starts_with("api/") || path == "api" {
        return not_found("route").into_response();
    }
    let candidate = if path.is_empty() { "index.html" } else { path };
    if let Some(file) = UiDist::get(candidate) {
        return asset_response(candidate, file);
    }
    match UiDist::get("index.html") {
        Some(file) => asset_response("index.html", file),
        None => Html(NOT_BUILT_PAGE).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn project_path_status_reports_an_unborn_repository_as_importable() {
        let path =
            std::env::temp_dir().join(format!("orx-project-path-status-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        let status = std::process::Command::new("git")
            .current_dir(&path)
            .args(["init", "-q", "-b", "main"])
            .status()
            .unwrap();
        assert!(status.success());

        let response = project_path_status(Query(ProjectPathStatusQ {
            path: Some(path.to_string_lossy().into_owned()),
        }))
        .await;
        let Json(body) = match response {
            Ok(body) => body,
            Err(error) => panic!("unexpected path status error: {}", error.1),
        };

        assert_eq!(body["gitState"], "unborn");
        assert_eq!(body["initialized"], true);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn plan_never_becomes_the_preferred_mode_for_new_sessions() {
        assert_eq!(
            preferred_permission_mode("claude-code", Some("plan".into())).as_deref(),
            Some("auto")
        );
        assert_eq!(
            preferred_permission_mode("codex", Some("plan".into())).as_deref(),
            Some("approve-for-me")
        );
        assert_eq!(
            preferred_permission_mode("opencode", Some("plan".into())).as_deref(),
            Some("default")
        );
    }

    fn expect_profile(
        result: std::result::Result<crate::telemetry::ResearchProfile, ApiError>,
    ) -> crate::telemetry::ResearchProfile {
        match result {
            Ok(profile) => profile,
            Err(error) => panic!("unexpected profile error: {}", error.1),
        }
    }

    #[test]
    fn research_profile_requires_an_area_and_other_description() {
        let missing_area = normalize_research_profile(vec![], None, None, vec![]).unwrap_err();
        assert_eq!(missing_area.1, "choose at least one research area");

        let missing_other =
            normalize_research_profile(vec!["Other".into()], Some("   ".into()), None, vec![])
                .unwrap_err();
        assert_eq!(
            missing_other.1,
            "describe your research area when choosing Other"
        );
    }

    #[test]
    fn research_profile_preserves_disclosed_free_text() {
        let profile = expect_profile(normalize_research_profile(
            vec!["AI/ML".into(), "Other".into()],
            Some("  AI for theorem proving  ".into()),
            Some("  I study RL.\nSecond line.  ".into()),
            vec![crate::telemetry::ProfilePaper {
                paper_id: "1706.03762".into(),
                title: Some("Attention Is All You Need".into()),
            }],
        ));

        assert_eq!(
            profile.other_area.as_deref(),
            Some("  AI for theorem proving  ")
        );
        assert_eq!(
            profile.background.as_deref(),
            Some("  I study RL.\nSecond line.  ")
        );
        assert_eq!(
            profile.papers[0].title.as_deref(),
            Some("Attention Is All You Need")
        );
    }

    #[test]
    fn legacy_profile_update_preserves_saved_research_areas() {
        let current = crate::telemetry::ResearchProfile {
            research_areas: vec!["Physics".into(), "Other".into()],
            other_area: Some("Quantum information".into()),
            ..crate::telemetry::ResearchProfile::default()
        };
        let updated = expect_profile(profile_settings_update(
            SetProfileReq {
                research_areas: vec![],
                other_area: None,
                background: Some("Updated background".into()),
                papers: vec![],
            },
            current,
        ));

        assert_eq!(updated.research_areas, vec!["Physics", "Other"]);
        assert_eq!(updated.other_area.as_deref(), Some("Quantum information"));
        assert_eq!(updated.background.as_deref(), Some("Updated background"));
    }

    #[test]
    fn api_run_exposes_cancel_intent() {
        let run = StoredRun {
            id: "run-1".into(),
            experiment_id: "experiment-1".into(),
            project_id: "project-1".into(),
            status: "running".into(),
            backend_json: "{}".into(),
            command: String::new(),
            created_at: 1,
            updated_at: 2,
            ended_at: None,
            exit_code: None,
            commit_sha: None,
            result_markdown: None,
            cancel_requested: true,
            chat_session_id: None,
        };

        let value = serde_json::to_value(ApiRun::from(&run)).unwrap();
        assert_eq!(value["cancelRequested"], true);
    }

    #[test]
    fn project_json_exposes_artifacts_dir_with_legacy_alias() {
        let project = local::model::LocalProject {
            id: "p1".into(),
            name: "Demo".into(),
            slug: "demo".into(),
            github_owner: "o".into(),
            github_repo: "r".into(),
            github_sync_enabled: true,
            baseline_branch: "main".into(),
            repo_path: "/tmp/r".into(),
            run_command: None,
            paper_id: None,
            created_at: 0,
            updated_at: 0,
        };
        let json = project_json_with_artifacts_dir(
            &project,
            "/tmp/openresearch-test/files/demo".to_string(),
        );
        assert_eq!(json["artifactsDir"], json["filesDir"]);
        assert!(json["artifactsDir"]
            .as_str()
            .unwrap()
            .ends_with("files/demo"));
    }

    #[test]
    fn project_file_text_never_lossily_decodes_binary_bytes() {
        let (content, binary) = decode_project_file_text(b"plain text\n".to_vec(), false);
        assert_eq!(content, "plain text\n");
        assert!(!binary);

        for bytes in [b"header\0payload".to_vec(), vec![0x89, b'P', b'N', b'G']] {
            let (content, binary) = decode_project_file_text(bytes, false);
            assert!(content.is_empty());
            assert!(binary);
        }

        let mut split_utf8 = b"prefix".to_vec();
        split_utf8.push(0xe2);
        let (content, binary) = decode_project_file_text(split_utf8, true);
        assert_eq!(content, "prefix");
        assert!(!binary);
    }

    #[test]
    fn project_file_paths_reject_traversal() {
        assert_eq!(
            validated_project_file_path("./figures/chart.png")
                .map_err(|error| error.1)
                .unwrap()
                .0,
            "figures/chart.png"
        );
        for path in ["", "../secret", "/etc/passwd", "figures/../secret"] {
            assert!(
                validated_project_file_path(path).is_err(),
                "accepted {path:?}"
            );
        }
    }

    // ApiError has no Debug, so `.unwrap()` on the Err path won't compile; drop
    // the error to its message string to make the Result assertion-friendly.
    fn abs_path(path: &str) -> std::result::Result<(String, std::path::PathBuf), String> {
        validated_absolute_file_path(path).map_err(|error| error.1)
    }

    #[test]
    fn absolute_file_paths_require_an_absolute_path() {
        assert_eq!(
            abs_path("  /etc/hosts  "),
            Ok((
                "/etc/hosts".to_string(),
                std::path::PathBuf::from("/etc/hosts")
            )),
        );
        for path in ["", "   ", "relative/path", "../secret", &"/x".repeat(3000)] {
            assert!(abs_path(path).is_err(), "accepted {path:?}");
        }
    }

    #[test]
    fn absolute_file_paths_expand_a_leading_tilde() {
        let home = dirs::home_dir().expect("home dir");
        // `~/x` and bare `~` resolve under home; the display stays as typed.
        assert_eq!(
            abs_path("~/.ssh/config"),
            Ok(("~/.ssh/config".to_string(), home.join(".ssh/config"))),
        );
        assert_eq!(abs_path("~").map(|(_, p)| p), Ok(home));
        // `~otheruser` isn't expanded, so it stays relative and is rejected.
        assert!(abs_path("~otheruser/x").is_err());
    }

    fn no_key(path: Option<&str>) -> SshReadiness {
        SshReadiness::NoUsableKey {
            pub_path: path.map(str::to_string),
        }
    }

    /// The row used to hardcode `~/.ssh/id_ed25519.pub`, which is wrong on any
    /// machine whose key is named something else.
    #[test]
    fn names_the_key_file_we_actually_found() {
        let s = openresearch_summary(true, &no_key(Some("~/.ssh/work_ed25519.pub")));
        assert!(s.contains("orx ssh-key add ~/.ssh/work_ed25519.pub"));
        assert!(!s.contains("id_ed25519"), "no guessed default");
    }

    /// Nothing on disk to register, so `ssh-key add` alone would fail.
    #[test]
    fn tells_you_to_generate_one_when_there_is_no_key() {
        let s = openresearch_summary(true, &no_key(None));
        assert!(s.contains("ssh-keygen"));
    }

    #[test]
    fn signed_out_beats_every_key_state() {
        assert!(openresearch_summary(false, &SshReadiness::Ready).contains("orx login"));
        assert!(openresearch_summary(false, &no_key(None)).contains("orx login"));
    }

    #[test]
    fn unverified_says_why_it_could_not_check() {
        let s = openresearch_summary(
            true,
            &SshReadiness::Unverified {
                reason: "timed out".to_string(),
            },
        );
        assert!(s.contains("timed out"), "surfaces the cause: {s}");
    }
}
