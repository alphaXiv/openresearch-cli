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

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderName, Method, StatusCode, Uri};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use base64::Engine as _;
use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::error::{anyhow, Result};
use crate::local::opencode::AgentHost;
use crate::local;
use crate::store::{log_path, Store, StoredRun};
use crate::{browser, UpArgs};

pub async fn run(args: UpArgs) -> Result<()> {
    let port = args.port;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| anyhow!("Could not bind 127.0.0.1:{}: {}", port, e))?;
    // Open early so the schema exists before any request or agent spawn.
    let store = Store::open()?;

    let agent = Arc::new(AgentHost::new(args.model.clone()));
    let state = AppState {
        agent: agent.clone(),
        // Timeout-free client: the proxy carries long-lived SSE bodies.
        http: reqwest::Client::new(),
    };

    if !args.no_agent {
        let projects = store.list_local_projects()?;
        if projects.len() == 1 {
            let project = projects.into_iter().next().unwrap();
            let agent = agent.clone();
            // Spawn in the background: ensure() clones/fetches + health-polls
            // for up to 30s and must not delay the dashboard coming up.
            tokio::spawn(async move {
                if let Err(err) = agent.ensure(&project).await {
                    eprintln!("orx up: could not start the agent: {err}");
                }
            });
        }
    }

    spawn_hf_preflight();

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
    http: reqwest::Client,
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/projects", get(list_projects).post(create_project))
        .route("/api/projects/{id}", get(get_project).patch(update_project))
        .route(
            "/api/projects/{id}/experiments",
            get(list_experiments).post(create_experiment),
        )
        .route("/api/projects/{id}/runs", get(list_project_runs))
        .route("/api/experiments/{id}/run", post(run_experiment))
        .route("/api/runs/{id}/cancel", post(cancel_run))
        .route("/api/runs/{id}/log", get(run_log))
        .route("/api/events", get(events))
        .route("/api/settings/hf", get(hf_settings).post(set_hf_token))
        .route("/api/agent/status", get(agent_status))
        .route("/api/agent/ensure", post(agent_ensure))
        .route("/opencode/{*path}", any(proxy_opencode))
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

async fn list_projects() -> ApiResult {
    let projects = Store::open()?.list_local_projects()?;
    Ok(Json(json!({ "projects": projects })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateProjectReq {
    name: String,
    github_owner: String,
    github_repo: String,
    baseline_branch: Option<String>,
    run_command: Option<String>,
}

async fn create_project(Json(req): Json<CreateProjectReq>) -> ApiResult {
    if req.name.trim().is_empty()
        || req.github_owner.trim().is_empty()
        || req.github_repo.trim().is_empty()
    {
        return Err(bad_request("name, githubOwner and githubRepo are required"));
    }
    // The clone shells out to git (network); keep it off the async workers.
    let project = tokio::task::spawn_blocking(move || {
        let store = Store::open()?;
        local::projects::create_project(
            &store,
            req.name.trim(),
            req.github_owner.trim(),
            req.github_repo.trim(),
            req.baseline_branch,
            req.run_command,
        )
        .map(|(project, _baseline)| project)
    })
    .await
    .map_err(|e| anyhow!("clone task failed: {e}"))?
    .map_err(bad_request)?;
    Ok(Json(json!({ "project": project })))
}

async fn get_project(Path(id): Path<String>) -> ApiResult {
    let project = Store::open()?
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
        return Err(bad_request("nothing to update: pass name and/or runCommand"));
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateExperimentReq {
    parent_experiment_id: Option<String>,
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
            // No parent -> the project root; never a second root.
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
    flavor: Option<String>,
    timeout: Option<String>,
}

async fn run_experiment(Path(id): Path<String>, body: Bytes) -> ApiResult {
    // Tolerate an empty body — flavor/timeout are both optional in the schema.
    let req: RunReq = if body.is_empty() {
        RunReq::default()
    } else {
        serde_json::from_slice(&body).map_err(bad_request)?
    };
    let args = crate::ExpRunArgs {
        exp_id: id,
        gpu: None,
        count: None,
        disk: None,
        provider: None,
        cpu: None,
        vcpus: None,
        sandbox: None,
        backend: Some("hf".to_string()),
        flavor: req.flavor,
        image: None,
        timeout: req.timeout,
        force: false,
    };
    // Same code path as CLI `orx exp run --backend hf` on a local experiment.
    let run = local::hf::submit_local_hf(&args).await.map_err(bad_request)?;
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

// --- agent ----------------------------------------------------------------

async fn agent_status(State(state): State<AppState>) -> Json<Value> {
    Json(json!(state.agent.status().await))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnsureReq {
    project_id: String,
}

async fn agent_ensure(State(state): State<AppState>, Json(req): Json<EnsureReq>) -> ApiResult {
    let project = Store::open()?
        .get_local_project(&req.project_id)?
        .ok_or_else(|| not_found("project"))?;
    let status = state.agent.ensure(&project).await.map_err(bad_request)?;
    Ok(Json(json!(status)))
}

// --- /opencode/* reverse proxy --------------------------------------------

fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
    )
}

async fn proxy_opencode(
    State(state): State<AppState>,
    Path(path): Path<String>,
    req: axum::extract::Request,
) -> Response {
    let Some(port) = state.agent.proxy_port().await else {
        return ApiError(
            StatusCode::BAD_GATEWAY,
            "agent not running — POST /api/agent/ensure first".to_string(),
        )
        .into_response();
    };
    let query = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let url = format!("http://127.0.0.1:{port}/{path}{query}");

    let (parts, body) = req.into_parts();
    let mut rb = state.http.request(parts.method.clone(), url);
    for (name, value) in parts.headers.iter() {
        // host: reqwest sets it for the target. content-length: the streamed
        // body goes out chunked, a stale length would corrupt the request.
        if !is_hop_by_hop(name) && name != header::HOST && name != header::CONTENT_LENGTH {
            rb = rb.header(name, value);
        }
    }
    if !matches!(parts.method, Method::GET | Method::HEAD) {
        rb = rb.body(reqwest::Body::wrap_stream(body.into_data_stream()));
    }

    let upstream = match rb.send().await {
        Ok(r) => r,
        Err(err) => {
            return ApiError(StatusCode::BAD_GATEWAY, format!("opencode proxy: {err}"))
                .into_response();
        }
    };

    let mut builder = Response::builder().status(upstream.status());
    for (name, value) in upstream.headers().iter() {
        if !is_hop_by_hop(name) {
            builder = builder.header(name, value);
        }
    }
    // Stream the body through unbuffered — this is what keeps SSE live.
    builder
        .body(Body::from_stream(upstream.bytes_stream()))
        .unwrap_or_else(|err| {
            ApiError(StatusCode::BAD_GATEWAY, format!("opencode proxy: {err}")).into_response()
        })
}

// --- /api/events SSE ------------------------------------------------------

async fn events() -> Sse<impl Stream<Item = std::result::Result<Event, Infallible>>> {
    // Small buffer on purpose: run.log events can carry ~MB payloads, and a
    // stalled client must backpressure the loop, not queue hundreds of MB.
    let (tx, rx) = mpsc::channel::<Event>(16);
    tokio::spawn(event_loop(tx));
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
            cursor.projects.insert(project.id.clone(), project.updated_at);
            out.push(json_event("project.updated", &json!({ "project": project })));
        }
        push_experiment_events(&store, &project.id, cursor, &mut out)?;
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

fn push_log_delta(run: &StoredRun, cursor: &mut EventCursor, out: &mut Vec<Event>, budget: &mut u64) {
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
    (
        [(header::CONTENT_TYPE, mime_for(path))],
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
