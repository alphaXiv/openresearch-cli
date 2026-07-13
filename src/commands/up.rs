//! `orx up` — the local autoresearch dashboard server.
//!
//! One axum process on 127.0.0.1 serving three surfaces:
//!   /            embedded SPA (rust-embed over ui/dist, index.html fallback)
//!   /api/*       JSON over the local SQLite store + run-log files
//!   /api/events  SSE: 500ms store + log-file diff loop (serve.rs idiom)
//!   /opencode/*  streaming reverse proxy to the locally spawned `opencode serve`
//!
//! Fully local: no client.rs / OpenResearch api anywhere on these paths. No
//! auth — the bind is loopback-only.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode, Uri};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
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
use crate::store::{log_path, now_ms, SshHostTest, Store, StoredChatSession, StoredRun};
use crate::{browser, UpArgs};

pub async fn run(args: UpArgs) -> Result<()> {
    let port = args.port;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| anyhow!("Could not bind 127.0.0.1:{}: {}", port, e))?;
    // Open early so the schema exists before any request or agent spawn.
    Store::open()?;

    // Harnesses spawn lazily on the first message to one of their sessions;
    // no eager agent bring-up. (--no-agent is now a no-op kept for compat.)
    let agent = Arc::new(AgentHost::new(args.model.clone()));
    let state = AppState {
        agent: agent.clone(),
        chat: Arc::new(ChatHost::new(agent.clone())),
        harnesses: Arc::new(tokio::sync::Mutex::new(None)),
    };

    spawn_hf_preflight();
    spawn_k8s_preflight();
    spawn_agent_git_preflight();
    // Wake an idle chat session when a run completes (the agent's wait loop
    // covers the busy case; this covers turns that ended early).
    tokio::spawn(local::chat::watch_runs(state.chat.clone()));

    let app = router(state);
    let url = format!("http://127.0.0.1:{port}");
    eprintln!("orx up: dashboard on {url}");
    if !args.no_browser {
        browser::open_browser(&url);
    }

    // select! instead of graceful shutdown: open SSE streams never complete,
    // so waiting on connections would hang Ctrl-C forever.
    tokio::select! {
        r = axum::serve(listener, app) => r.map_err(|e| anyhow!("orx up: server error: {e}"))?,
        _ = tokio::signal::ctrl_c() => eprintln!("orx up: shutting down"),
    }
    agent.shutdown().await;
    Ok(())
}

#[derive(Clone)]
struct AppState {
    agent: Arc<AgentHost>,
    chat: Arc<ChatHost>,
    /// Harness detection cache — detection shells out to CLIs, so it's rate-
    /// limited to once per TTL unless the UI asks for a refresh.
    harnesses: Arc<tokio::sync::Mutex<Option<(std::time::Instant, Value)>>>,
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/projects", get(list_projects).post(create_project))
        .route(
            "/api/projects/{id}",
            get(get_project)
                .patch(update_project)
                .delete(delete_project),
        )
        .route("/api/projects/{id}/open", post(open_project))
        .route(
            "/api/projects/{id}/experiments",
            get(list_experiments).post(create_experiment),
        )
        .route("/api/projects/{id}/runs", get(list_project_runs))
        .route("/api/instances", get(list_instances))
        .route("/api/experiments/{id}/run", post(run_experiment))
        .route("/api/runs/{id}/cancel", post(cancel_run))
        .route("/api/runs/{id}/log", get(run_log))
        .route("/api/runs/{id}/diff", get(run_diff))
        .route("/api/experiments/{id}/commits", get(experiment_commits))
        .route(
            "/api/experiments/{id}/commits/{sha}/diff",
            get(experiment_commit_diff),
        )
        .route("/api/projects/{id}/working-tree", get(project_working_tree))
        .route("/api/projects/{id}/file", get(project_file))
        .route(
            "/api/projects/{id}/files",
            get(list_files).delete(delete_file),
        )
        .route("/api/projects/{id}/files/report", get(file_report))
        .route("/api/projects/{id}/files/file", get(serve_file))
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
            "/api/settings/git",
            get(git_settings).post(set_git_settings),
        )
        .route(
            "/api/settings/git/token",
            post(set_git_token).delete(delete_git_token),
        )
        .route("/api/settings/ssh", get(ssh_settings))
        .route("/api/settings/ssh/preflight", post(ssh_preflight))
        .route(
            "/api/settings/slurm",
            get(slurm_settings).post(set_slurm_settings),
        )
        .route("/api/settings/slurm/preflight", post(slurm_preflight))
        .route("/api/harnesses", get(list_harnesses))
        .route("/api/skills", get(list_skills))
        .route(
            "/api/chat/sessions",
            get(list_chat_sessions).post(create_chat_session),
        )
        .route(
            "/api/chat/sessions/{id}",
            axum::routing::delete(delete_chat_session),
        )
        .route("/api/chat/sessions/{id}/messages", get(chat_messages))
        .route("/api/chat/sessions/{id}/message", post(send_chat_message))
        .route("/api/chat/sessions/{id}/interrupt", post(interrupt_chat))
        .route("/api/chat/sessions/{id}/respond", post(respond_chat))
        .route("/api/chat/attachments/{name}", get(chat_attachment))
        .route("/api/agent/status", get(agent_status))
        .fallback(spa)
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
/// object and internal fields (cancel intent) dropped.
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
        }
    }
}

// --- basic routes ---------------------------------------------------------

async fn health() -> Json<Value> {
    Json(json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }))
}

/// Slash-skills the composer's `/` dropdown offers (expanded server-side).
async fn list_skills() -> Json<Value> {
    let skills: Vec<Value> = crate::local::skills::CATALOG
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "description": s.description,
                "argHint": s.arg_hint,
            })
        })
        .collect();
    Json(json!({ "skills": skills }))
}

async fn list_projects() -> ApiResult {
    let projects = Store::open()?.list_local_projects()?;
    Ok(Json(json!({ "projects": projects })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateProjectReq {
    name: String,
    github_owner: Option<String>,
    github_repo: Option<String>,
    baseline_branch: Option<String>,
    run_command: Option<String>,
    /// Create a blank private repo named after the project on the user's
    /// GitHub account instead of pointing at an existing one.
    #[serde(default)]
    create_repo: bool,
    /// Fork-by-copy the entered repo into a fresh `<repo>-<hash>` repo on the
    /// user's account. Also applied automatically when the user lacks push
    /// access to the entered repo — experiments need somewhere to push.
    #[serde(default)]
    fork_repo: bool,
}

async fn create_project(Json(req): Json<CreateProjectReq>) -> ApiResult {
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return Err(bad_request("name is required"));
    }
    let (owner, repo, baseline_branch) = if req.create_repo {
        let (owner, repo, default_branch) = local::github::create_user_repo(&local::slugify(&name))
            .await
            .map_err(bad_request)?;
        (owner, repo, Some(default_branch))
    } else {
        let owner = req.github_owner.unwrap_or_default().trim().to_string();
        let repo = req.github_repo.unwrap_or_default().trim().to_string();
        if owner.is_empty() || repo.is_empty() {
            return Err(bad_request("githubOwner and githubRepo are required"));
        }
        let branch = req.baseline_branch.filter(|b| !b.trim().is_empty());
        // Unknown access (no token / API hiccup) counts as access: forking
        // needs a token anyway, and surprise forks are worse than a later
        // push error.
        let fork = req.fork_repo
            || !local::github::has_push_access(&owner, &repo)
                .await
                .unwrap_or(true);
        if fork {
            // The entered branch picks what gets copied; the fork itself
            // starts at its default branch.
            let (owner, repo, default_branch) =
                local::github::fork_copy_repo(&owner, &repo, branch)
                    .await
                    .map_err(bad_request)?;
            (owner, repo, Some(default_branch))
        } else {
            (owner, repo, branch)
        }
    };
    // The clone shells out to git (network); keep it off the async workers.
    let run_command = req.run_command;
    let clone = move || {
        let store = Store::open()?;
        local::projects::create_project(&store, &name, &owner, &repo, baseline_branch, run_command)
    };
    let mut result = tokio::task::spawn_blocking(clone.clone())
        .await
        .map_err(|e| anyhow!("clone task failed: {e}"))?;
    // A just-created repo can lag a beat before its auto-init commit is
    // cloneable; one retry covers it (the clone step is idempotent).
    if result.is_err() && req.create_repo {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        result = tokio::task::spawn_blocking(clone)
            .await
            .map_err(|e| anyhow!("clone task failed: {e}"))?;
    }
    Ok(Json(json!({ "project": result.map_err(bad_request)? })))
}

async fn get_project(Path(id): Path<String>) -> ApiResult {
    let project = Store::open()?
        .get_local_project(&id)?
        .ok_or_else(|| not_found("project"))?;
    Ok(Json(json!({ "project": project })))
}

/// Mark a project visited: bumps updated_at, which drives the recency sort
/// and the SSE project.updated diff.
async fn open_project(Path(id): Path<String>) -> ApiResult {
    let store = Store::open()?;
    store.touch_local_project(&id)?;
    let project = store
        .get_local_project(&id)?
        .ok_or_else(|| not_found("project"))?;
    Ok(Json(json!({ "project": project })))
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

async fn update_project(Path(id): Path<String>, Json(req): Json<UpdateProjectReq>) -> ApiResult {
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
    Ok(Json(json!({ "project": project })))
}

/// Delete a project and everything hanging off it. Refuses while runs are in
/// flight (deleting their rows would strand the supervisor mid-job) — but
/// requests their cancellation, so a retry shortly after goes through. The
/// GitHub repo and the cache clone are left untouched.
async fn delete_project(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
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
        for run in &in_flight {
            let _ = store.set_cancel_requested(&run.id, true);
        }
        return Err(bad_request(format!(
            "{} run(s) still in flight — cancellation requested; retry once they stop",
            in_flight.len()
        )));
    }
    // Abort any in-flight chat turns before their rows disappear, and clean up
    // each session's serve child + worktree (the rows cascade with the project).
    for session in store.list_chat_sessions_by_project(&id)? {
        let _ = state.chat.interrupt(&session.id).await;
        state.chat.opencode.kill_session(&session.id).await;
        local::chat::cleanup_session_worktree(&project, &session.id);
    }
    store.delete_local_project(&id)?;
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateExperimentReq {
    parent_experiment_id: Option<String>,
    /// Force a new baseline root even when the project already has one.
    #[serde(default)]
    baseline: bool,
    slug: Option<String>,
    title: Option<String>,
    description: Option<String>,
    run_command: Option<String>,
}

async fn create_experiment(
    Path(id): Path<String>,
    Json(req): Json<CreateExperimentReq>,
) -> ApiResult {
    // Branch create + push shells out to git (network); off the async workers.
    let experiment = tokio::task::spawn_blocking(move || {
        let store = Store::open()?;
        let project = store
            .get_local_project(&id)?
            .ok_or_else(|| not_found("project"))?;
        let parent = match &req.parent_experiment_id {
            Some(pid) => Some(
                store
                    .get_local_experiment(pid)?
                    .ok_or_else(|| not_found("parent experiment"))?,
            ),
            // `baseline` forces a new root; otherwise no parent -> the oldest
            // project root when one exists (empty project: a new baseline).
            None if req.baseline => None,
            None => local::experiments::project_root(&store, &project.id)?,
        };
        local::experiments::create_experiment(
            &store,
            &project,
            parent.as_ref(),
            req.slug.as_deref(),
            req.title,
            req.description,
            req.run_command,
        )
        .map_err(bad_request)
    })
    .await
    .map_err(|e| ApiError::from(anyhow!("branch task failed: {e}")))??;
    Ok(Json(json!({ "experiment": experiment })))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RunReq {
    backend: Option<String>,
    flavor: Option<String>,
    /// Repo-relative manifest path (k8s only; default .orx/k8s.yaml).
    manifest: Option<String>,
    timeout: Option<String>,
    /// ssh config host alias of the Slurm login node (slurm only; defaults to
    /// the slurm settings' host).
    host: Option<String>,
}

async fn run_experiment(Path(id): Path<String>, body: Bytes) -> ApiResult {
    // Tolerate an empty body — every field is optional in the schema.
    let req: RunReq = if body.is_empty() {
        RunReq::default()
    } else {
        serde_json::from_slice(&body).map_err(bad_request)?
    };
    let backend = req.backend.as_deref().unwrap_or("hf").to_string();
    let args = crate::ExpRunArgs {
        exp_id: id,
        gpu: None,
        count: None,
        disk: None,
        provider: None,
        cpu: None,
        vcpus: None,
        sandbox: None,
        backend: Some(backend.clone()),
        flavor: req.flavor,
        host: req.host,
        manifest: req.manifest,
        image: None,
        timeout: req.timeout,
        force: false,
    };
    // Same code paths as CLI `orx exp run --backend <b>` on a local experiment.
    let run = match backend.as_str() {
        "hf" => local::hf::submit_local_hf(&args).await,
        "k8s" => local::k8s::submit_local_k8s(&args).await,
        "slurm" => local::slurm::submit_local_slurm(&args).await,
        other => Err(anyhow!(
            "Unknown backend '{other}'. Supported: hf, k8s, slurm."
        )),
    }
    .map_err(bad_request)?;
    Ok(Json(json!({ "run": ApiRun::from(&run) })))
}

async fn cancel_run(Path(id): Path<String>) -> ApiResult {
    let store = Store::open()?;
    let run = local::local_run(&store, &id)?.ok_or_else(|| not_found("run"))?;
    // A terminal run must not gain a stale cancel_requested flag.
    if is_terminal(&run.status) {
        return Err(bad_request(format!("run already {}", run.status)));
    }
    store.set_cancel_requested(&run.id, true)?;
    Ok(Json(json!({ "ok": true })))
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

/// Cumulative diff of a run's commit vs its experiment's parent branch —
/// "everything this experiment changed as of this run".
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
        let parent_id = exp
            .parent_experiment_id
            .ok_or_else(|| bad_request("baseline runs have no parent branch to diff against"))?;
        let parent = store
            .get_local_experiment(&parent_id)?
            .ok_or_else(|| not_found("parent experiment"))?;
        let project = store
            .get_local_project(&exp.project_id)?
            .ok_or_else(|| not_found("project"))?;
        let repo = std::path::Path::new(&project.repo_path);
        let payload = local::git::diff_range(repo, &parent.branch_name, &sha)?;
        Ok(Json(diff_json(payload)))
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

/// Cap on file bytes served to the viewer (mirrors openresearch.sh).
const FILE_READ_LIMIT: u64 = 512_000;

#[derive(Deserialize)]
struct ProjectFileQuery {
    path: String,
}

/// One file from the project's clone (the agent's working tree), for the UI
/// file viewer. Path is repo-relative; traversal outside the clone is rejected.
async fn project_file(Path(id): Path<String>, Query(q): Query<ProjectFileQuery>) -> ApiResult {
    blocking_api(move || {
        use std::io::Read as _;
        let rel = q.path.trim().trim_start_matches("./").to_string();
        if rel.is_empty() || rel.len() > 1024 {
            return Err(bad_request("invalid path"));
        }
        let rel_path = std::path::Path::new(&rel);
        let traversal = rel_path.is_absolute()
            || rel_path
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)));
        if traversal {
            return Err(bad_request("path must be repo-relative"));
        }

        let store = Store::open()?;
        let project = store
            .get_local_project(&id)?
            .ok_or_else(|| not_found("project"))?;
        let root = std::fs::canonicalize(&project.repo_path)
            .map_err(|e| ApiError::from(anyhow!("repo clone unavailable: {e}")))?;
        let not_found_json = json!({
            "path": rel, "content": "", "truncated": false, "notFound": true,
        });
        // Canonicalize so symlinks can't escape the clone.
        let full = match std::fs::canonicalize(root.join(rel_path)) {
            Ok(p) => p,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Json(not_found_json)),
            Err(e) => return Err(ApiError::from(anyhow!("read failed: {e}"))),
        };
        if !full.starts_with(&root) {
            return Err(bad_request("path escapes repository"));
        }
        if full.is_dir() {
            return Err(bad_request("path is a directory"));
        }
        let file = match std::fs::File::open(&full) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Json(not_found_json)),
            Err(e) => return Err(ApiError::from(anyhow!("read failed: {e}"))),
        };
        let mut buf = Vec::new();
        std::io::Read::take(file, FILE_READ_LIMIT + 1)
            .read_to_end(&mut buf)
            .map_err(|e| ApiError::from(anyhow!("read failed: {e}")))?;
        let truncated = buf.len() as u64 > FILE_READ_LIMIT;
        buf.truncate(FILE_READ_LIMIT as usize);
        Ok(Json(json!({
            "path": rel,
            "content": String::from_utf8_lossy(&buf).into_owned(),
            "truncated": truncated,
            "notFound": false,
        })))
    })
    .await
}

// --- files ----------------------------------------------------------------

/// Listing of the project's files dir — the filesystem is the source of
/// truth; this scans it fresh on every call (and creates it if missing).
/// Top-level folders named for an experiment slug carry that experiment
/// (title, branch, latest run status) so the tab can group by experiment.
async fn list_files(Path(id): Path<String>) -> ApiResult {
    blocking_api(move || {
        let store = Store::open()?;
        let project = store
            .get_local_project(&id)?
            .ok_or_else(|| not_found("project"))?;
        let experiments = store.list_experiments_by_project(&id)?;
        // Newest-first run list → first status seen per experiment is latest.
        let mut latest: HashMap<String, String> = HashMap::new();
        for run in store.list_runs_by_project(&id)? {
            latest.entry(run.experiment_id).or_insert(run.status);
        }
        let listing = local::files::list(&project, &experiments, &latest)?;
        Ok(Json(json!(listing)))
    })
    .await
}

#[derive(Deserialize)]
struct FilePathQuery {
    path: String,
}

/// A report folder's markdown body (`<name>/report.md`).
async fn file_report(Path(id): Path<String>, Query(q): Query<FilePathQuery>) -> ApiResult {
    blocking_api(move || {
        let store = Store::open()?;
        let project = store
            .get_local_project(&id)?
            .ok_or_else(|| not_found("project"))?;
        let markdown = local::files::read_report_markdown(&project, &q.path)
            .map_err(|_| not_found("report"))?;
        Ok(Json(json!({ "markdown": markdown })))
    })
    .await
}

/// Delete a file or report folder in the files dir, by relative path.
async fn delete_file(Path(id): Path<String>, Query(q): Query<FilePathQuery>) -> ApiResult {
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

/// Raw file bytes, by files-dir-relative path. `no-cache`: the same path can
/// be rewritten in place on disk.
async fn serve_file(
    Path(id): Path<String>,
    Query(q): Query<FilePathQuery>,
) -> std::result::Result<Response, ApiError> {
    tokio::task::spawn_blocking(move || {
        let store = Store::open()?;
        let project = store
            .get_local_project(&id)?
            .ok_or_else(|| not_found("project"))?;
        let bytes = local::files::read_file(&project, &q.path).map_err(|_| not_found("file"))?;
        let content_type = local::files::content_type_for_path(&q.path);
        Ok((
            [
                (header::CONTENT_TYPE, content_type),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            bytes,
        )
            .into_response())
    })
    .await
    .map_err(|e| ApiError::from(anyhow!("file task failed: {e}")))?
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

/// Startup warning when HF Jobs can't launch as-is. Never blocks startup.
fn spawn_hf_preflight() {
    tokio::spawn(async {
        let s = hf_token_status().await;
        let fix = "run `hf auth login` with a Jobs-write token, or set one in the dashboard Settings page";
        if !s.configured {
            eprintln!("orx up: warning: no Hugging Face token found — runs can't launch; {fix}.");
        } else if !s.valid {
            eprintln!(
                "orx up: warning: the Hugging Face token ({}) was rejected by huggingface.co; {fix}.",
                s.source.unwrap_or("unknown source")
            );
        } else if s.jobs_write == Some(false) {
            eprintln!(
                "orx up: warning: the Hugging Face token ({}) lacks Jobs write access — launches will fail; {fix}.",
                s.source.unwrap_or("unknown source")
            );
        }
    });
}

/// Startup summary of detected coding agents + git/GitHub credentials, so a
/// first-time CLI user learns autodetection happened at all. Never blocks.
fn spawn_agent_git_preflight() {
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
                "orx up: warning: no coding agent detected — install Claude Code, Codex or opencode and sign in to chat in the dashboard."
            );
        }
        // git checks shell out; keep them off the async workers.
        let _ = tokio::task::spawn_blocking(|| {
            let git = git_settings_json();
            if git.get("gitVersion").and_then(Value::as_str).is_none() {
                eprintln!("orx up: warning: git not found on PATH — cloning projects will fail.");
            } else if git.get("githubTokenSource").and_then(Value::as_str).is_none() {
                eprintln!(
                    "orx up: note: no GitHub credentials found (`gh auth login` or GITHUB_TOKEN) — private-repo clones and experiment-branch pushes need them unless SSH keys are set up."
                );
            }
        })
        .await;
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

/// Startup warning when a configured k8s backend can't launch as-is. Silent
/// when k8s was never configured — HF remains the default backend.
fn spawn_k8s_preflight() {
    tokio::spawn(async {
        let Ok(Some(settings)) = k8s::load_settings() else {
            return;
        };
        let p = k8s::preflight(settings.context.as_deref(), &settings.namespace).await;
        let fix = "check the cluster in the dashboard Settings → Compute page";
        if !p.kubectl_found {
            eprintln!("orx up: warning: kubectl not found on PATH — k8s runs can't launch.");
        } else if !p.reachable {
            eprintln!(
                "orx up: warning: Kubernetes cluster unreachable ({}) — k8s runs can't launch; {fix}.",
                p.error.unwrap_or_default()
            );
        } else if !p.can_create_jobs {
            eprintln!(
                "orx up: warning: no permission to create Jobs in namespace '{}' — k8s runs will fail; {fix}.",
                settings.namespace
            );
        }
    });
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
    // Spawn failure means gh isn't installed — distinct from installed-but-
    // signed-out, so the UI can lead with the right fix.
    let gh = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output();
    let gh_installed = gh.is_ok();
    let github_source = if std::env::var("GITHUB_TOKEN").is_ok_and(|t| !t.trim().is_empty()) {
        Some("env")
    } else if crate::config::synced_env_var("GITHUB_TOKEN").is_some() {
        Some("stored")
    } else {
        matches!(gh, Ok(out) if out.status.success() && !out.stdout.is_empty()).then_some("gh")
    };
    json!({
        "gitVersion": git_out(&["--version"]),
        "userName": git_out(&["config", "--global", "user.name"]),
        "userEmail": git_out(&["config", "--global", "user.email"]),
        "ghInstalled": gh_installed,
        "githubTokenSource": github_source,
    })
}

#[derive(Deserialize)]
struct SetGitTokenReq {
    token: String,
}

/// Validate a pasted GitHub token against the API, then persist it to the
/// synced env file — the same store job launches already read, so local git
/// ops and remote compute both pick it up.
async fn set_git_token(Json(req): Json<SetGitTokenReq>) -> ApiResult {
    let token = req.token.trim().to_string();
    if token.is_empty() {
        return Err(bad_request("token is required"));
    }
    let resp = reqwest::Client::new()
        .get("https://api.github.com/user")
        .header("User-Agent", "orx")
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| bad_request(format!("Could not reach api.github.com: {e}")))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(bad_request(
            "GitHub rejected the token — check it was copied fully.",
        ));
    }
    if !resp.status().is_success() {
        return Err(bad_request(format!(
            "GitHub returned {} validating the token.",
            resp.status()
        )));
    }
    // Classic PATs list scopes; fine-grained tokens send an empty header, so
    // only enforce when scopes are reported.
    let scopes = resp
        .headers()
        .get("x-oauth-scopes")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !scopes.trim().is_empty() && !scopes.split(',').any(|s| s.trim() == "repo") {
        return Err(bad_request(
            "Token is valid but lacks the `repo` scope — private clones and branch pushes would fail.",
        ));
    }
    tokio::task::spawn_blocking(move || {
        crate::config::write_synced_env_var("GITHUB_TOKEN", &token)?;
        Ok(Json(git_settings_json()))
    })
    .await
    .map_err(|e| ApiError::from(anyhow!("git task failed: {e}")))?
}

async fn delete_git_token() -> ApiResult {
    tokio::task::spawn_blocking(|| {
        crate::config::remove_synced_env_var("GITHUB_TOKEN")?;
        Ok(Json(git_settings_json()))
    })
    .await
    .map_err(|e| ApiError::from(anyhow!("git task failed: {e}")))?
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

/// Live check for one host: can we reach it (BatchMode ssh), and is `git` there?
async fn ssh_preflight(Json(req): Json<SshPreflightReq>) -> ApiResult {
    let host = req.host.trim().to_string();
    if host.is_empty() {
        return Err(bad_request("host is required"));
    }
    let p = crate::jobs::ssh::preflight(&host).await;
    let test = SshHostTest {
        host,
        reachable: p.reachable,
        git_found: p.git_found,
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

/// Live check for one login node: reachable, Slurm CLI + git present, and
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
        "gitFound": p.git_found,
        "partitions": p.partitions,
        "error": p.error,
    })))
}

// --- harnesses ---------------------------------------------------------------

const HARNESS_CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Deserialize)]
struct HarnessQuery {
    refresh: Option<u8>,
}

async fn list_harnesses(
    State(state): State<AppState>,
    Query(q): Query<HarnessQuery>,
) -> Json<Value> {
    let mut cache = state.harnesses.lock().await;
    if q.refresh != Some(1) {
        if let Some((at, payload)) = cache.as_ref() {
            if at.elapsed() < HARNESS_CACHE_TTL {
                return Json(payload.clone());
            }
        }
    }
    let payload = json!({ "harnesses": local::harness::detect_harnesses().await });
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
    reasoning_level: Option<String>,
}

async fn create_chat_session(Json(req): Json<CreateChatSessionReq>) -> ApiResult {
    if !local::harness::is_chat_harness(&req.harness) {
        return Err(bad_request(format!("unknown harness: {}", req.harness)));
    }
    let store = Store::open()?;
    store
        .get_local_project(&req.project_id)?
        .ok_or_else(|| not_found("project"))?;
    let nonempty = |s: Option<String>| s.filter(|v| !v.trim().is_empty());
    let session = StoredChatSession {
        id: format!("chat_{}", uuid::Uuid::new_v4()),
        project_id: req.project_id,
        harness: req.harness,
        native_session_id: None,
        title: None,
        model: nonempty(req.model),
        permission_mode: nonempty(req.permission_mode),
        reasoning_level: nonempty(req.reasoning_level),
        created_at: now_ms(),
        updated_at: now_ms(),
    };
    store.create_chat_session(&session)?;
    Ok(Json(
        json!({ "session": local::chat::session_json(&session, false) }),
    ))
}

async fn delete_chat_session(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    state.chat.delete_session(&id).await?;
    Ok(Json(json!({ "ok": true })))
}

async fn chat_messages(Path(id): Path<String>) -> ApiResult {
    Store::open()?
        .get_chat_session(&id)?
        .ok_or_else(|| not_found("chat session"))?;
    let messages = local::chat::list_messages(&id)?;
    Ok(Json(json!({ "messages": messages })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendChatReq {
    text: String,
    model: Option<String>,
    permission_mode: Option<String>,
    reasoning_level: Option<String>,
    #[serde(default)]
    images: Vec<local::chat::ImageAttachment>,
}

async fn send_chat_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SendChatReq>,
) -> ApiResult {
    let text = req.text.trim().to_string();
    if text.is_empty() && req.images.is_empty() {
        return Err(bad_request("text is required"));
    }
    let overrides = local::chat::TurnOverrides {
        model: req.model,
        permission_mode: req.permission_mode,
        reasoning_level: req.reasoning_level,
    };
    // The turn runs in the background; progress streams over /api/events.
    state
        .chat
        .send_message(&id, text, overrides, req.images)
        .await
        .map_err(bad_request)?;
    Ok(Json(json!({ "ok": true })))
}

/// Raw bytes of a pasted-image attachment, by bare file name.
async fn chat_attachment(Path(name): Path<String>) -> std::result::Result<Response, ApiError> {
    // Names are server-minted (img_<uuid>.<ext>); anything else is rejected.
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        || name.contains("..")
    {
        return Err(bad_request("invalid attachment name"));
    }
    tokio::task::spawn_blocking(move || {
        let path = local::chat::attachments_dir()?.join(&name);
        let bytes = std::fs::read(&path).map_err(|_| not_found("attachment"))?;
        Ok((
            [
                (
                    header::CONTENT_TYPE,
                    local::chat::attachment_content_type(&name),
                ),
                (header::CACHE_CONTROL, "max-age=31536000, immutable"),
            ],
            bytes,
        )
            .into_response())
    })
    .await
    .map_err(|e| ApiError::from(anyhow!("attachment task failed: {e}")))?
}

async fn interrupt_chat(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    state.chat.interrupt(&id).await?;
    Ok(Json(json!({ "ok": true })))
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
        })
        .await
        .map_err(bad_request)?;
    Ok(Json(json!({ "ok": true })))
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
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|ev| (Ok(ev), rx))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Diff state for one SSE subscriber.
#[derive(Default)]
struct EventCursor {
    projects: HashMap<String, i64>,
    experiments: HashMap<String, i64>,
    files: HashMap<String, u64>,
    runs: HashMap<String, (String, i64)>,
    log_offsets: HashMap<String, u64>,
}

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
    let store = Store::open()?;
    let mut out = Vec::new();
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
                &json!({ "project": project }),
            ));
        }
        push_experiment_events(&store, &project.id, cursor, &mut out)?;
        // Files appear live — anything written into the files dir (by the
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
    // are immutable by name.
    let cache = if path == "index.html" {
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
