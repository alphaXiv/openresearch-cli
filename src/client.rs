//! HTTP client for the OpenResearch API.
//!
//! JSON field names use `serde(rename_all = "camelCase")` so the wire
//! format matches the API exactly. The `request` helper surfaces errors as:
//!   - network failure  -> `Could not reach the API at {url}: ...`
//!   - HTTP 401         -> `Unauthorized — your token is invalid or revoked. Run `orx login` again.`
//!   - other non-2xx    -> `Request to {path} failed ({status} {reason}): {body}`
//!
//! All endpoint fns are `async` and take `&Credentials` as the first argument,
//! matching how commands call them.

use std::sync::OnceLock;

use reqwest::{Client, Method};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Credentials;
use crate::error::{anyhow, Result};

// ---------------------------------------------------------------------------
// Response DTOs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Org {
    pub id: String,
    pub name: String,
    pub created_by: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: String,
    pub name: String,
    pub description: String,
    pub archived: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Experiment {
    pub id: String,
    pub project_id: String,
    /// `null` for root experiments.
    pub parent_experiment_id: Option<String>,
    pub slug: String,
    pub title: String,
    /// Free-form notes / write-up for the experiment; empty string when unset.
    #[serde(default)]
    pub description: String,
    pub status: String,
    pub run_command: String,
    /// `null` until the experiment has been linked to a sandbox.
    #[serde(default)]
    pub sandbox_id: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub id: String,
    pub experiment_id: String,
    pub command: String,
    pub status: String,
    pub commit_sha: Option<String>,
    pub updated_at: String,
}

/// A single GPU offer from the compute catalog (`GET /compute/catalog`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuOffer {
    pub provider: String,
    pub offer_id: String,
    pub gpu: String,
    pub gpu_count: i64,
    /// Effective vCPUs allocated to the instance.
    pub vcpus: f64,
    /// System RAM in GB.
    pub ram_gb: f64,
    pub price_per_hour: f64,
    /// Disk storage rate while running, USD per GB per hour.
    pub disk_per_gb_hour: f64,
    pub region: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListCatalog {
    pub offers: Vec<GpuOffer>,
}

/// Response of `GET /experiments/{id}`: the experiment plus its most recent run.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetExperimentResult {
    pub experiment: Experiment,
    /// `null` when the experiment has never been run.
    pub latest_run: Option<Run>,
}

/// Mirrors the TS `"degraded" | "ready" | "warming"` union.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SyncStatus {
    Degraded,
    Ready,
    Warming,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectQueryResult {
    pub columns: Vec<String>,
    /// Each row is a list of arbitrary JSON cell values (`unknown[][]`).
    pub rows: Vec<Vec<Value>>,
    pub row_count: i64,
    pub total_row_count: i64,
    pub more_rows_available: bool,
    pub sync_status: SyncStatus,
    pub sync_errors: Vec<String>,
    pub last_synced_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WandbSummary {
    pub label: String,
    pub n: i64,
    pub min: f64,
    pub max: f64,
    pub last: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WandbFailed {
    pub label: String,
    pub error: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WandbChartResult {
    /// `null` when no run produced any points.
    pub chart_id: Option<String>,
    /// Presigned PNG URL, or `null` when nothing was rendered.
    pub url: Option<String>,
    pub metric_key: String,
    pub summaries: Vec<WandbSummary>,
    pub failed: Vec<WandbFailed>,
}

/// Mirrors the TS `DevSession`: `state: "none" | "provisioning" | "online" | "offline"`.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DevSessionState {
    None,
    Provisioning,
    Online,
    Offline,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DevSession {
    pub state: DevSessionState,
    pub sandbox_id: Option<String>,
}

/// `DevStatus extends DevSession` with a `dirty` list of changed paths.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DevStatus {
    pub state: DevSessionState,
    pub sandbox_id: Option<String>,
    pub dirty: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DevCloseResult {
    pub committed: bool,
    pub commit_sha: Option<String>,
    pub torn_down: bool,
}

/// Tagged union of dev filesystem operations. The discriminant is the JSON
/// field `op`, and the variant payloads use snake_case field names exactly as
/// the TS `DevFsOp` (e.g. `old_string`, `new_string`), so we override the
/// container's camelCase for these.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DevFsOp {
    Read {
        path: String,
    },
    Write {
        path: String,
        content: String,
    },
    StrReplace {
        path: String,
        old_string: String,
        new_string: String,
    },
    List {
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    Search {
        query: String,
    },
    Delete {
        path: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsOutput {
    pub output: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunLogExcerpt {
    pub content: String,
    pub start_byte: i64,
    pub end_byte: i64,
    pub total_bytes: i64,
    pub source: String,
    pub truncated_before: bool,
    pub truncated_after: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogSearchMatchingLine {
    pub line_number: i64,
    pub start_byte: i64,
    pub end_byte: i64,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogSearchRunResult {
    pub run_id: String,
    pub match_count: i64,
    pub total_lines: i64,
    pub source: String,
    pub matching_lines: Vec<LogSearchMatchingLine>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogSearchResult {
    pub capped: bool,
    pub pattern: String,
    pub results: Vec<LogSearchRunResult>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkdirFile {
    pub path: String,
    pub size: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkdirLs {
    pub files: Vec<WorkdirFile>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkdirRead {
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactExcerpt {
    pub content: String,
    pub key: String,
    pub start_byte: i64,
    pub end_byte: i64,
    pub total_bytes: i64,
    pub truncated_before: bool,
    pub truncated_after: bool,
}

/// One artifact uploaded during a run (`GET /runs/{id}/artifacts`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunArtifact {
    pub key: String,
    pub size: i64,
    /// Presigned download URL.
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListArtifacts {
    pub artifacts: Vec<RunArtifact>,
}

/// One W&B run linked to an OpenResearch run (`GET /runs/{id}/wandb-runs`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WandbRunLink {
    pub base_url: String,
    pub entity: String,
    pub project: String,
    pub wandb_run_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListWandbRuns {
    pub wandb_runs: Vec<WandbRunLink>,
}

/// Cumulative unified diff for a run (`GET /runs/{id}/diff`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunDiff {
    pub diff: String,
    pub truncated: bool,
    pub bytes_read: i64,
    pub byte_limit: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillRef {
    pub name: String,
    pub description: String,
    pub path: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListSkills {
    pub skills: Vec<SkillRef>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillContent {
    pub content: String,
}

// Thin envelope DTOs for the list endpoints.

#[derive(Debug, Clone, Deserialize)]
pub struct ListOrgs {
    pub orgs: Vec<Org>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListProjects {
    pub projects: Vec<Project>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListExperiments {
    pub experiments: Vec<Experiment>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListRuns {
    pub runs: Vec<Run>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExperimentEnvelope {
    pub experiment: Experiment,
}

// ---------------------------------------------------------------------------
// Request bodies (mirroring the inline TS body shapes)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WandbRunSpec {
    pub run_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WandbChartBody {
    pub metric_key: String,
    pub runs: Vec<WandbRunSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smoothing: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateChildBody {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parent_experiment_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateEmptyBaselineBody {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportBaselineBody {
    pub repo_full_name: String,
    #[serde(rename = "ref")]
    pub ref_: String,
    /// Always `null` in the TS caller; serialized as JSON `null`.
    pub patch: Option<Value>,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

// The TS field is literally `ref` (a Rust keyword), so the struct field is
// `ref_` with `#[serde(rename = "ref")]` to emit `ref` on the wire.

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DevCloseBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discard: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchLogsBody {
    pub pattern: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub experiment_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_matching_lines: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
struct QueryBody<'a> {
    sql: &'a str,
}

/// PATCH body for `update_experiment`. Only the fields the CLI sets are
/// included; every field is optional and omitted when `None`.
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct UpdateExperimentBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The `target` of a run launch (`POST /experiments/{id}/run`). Internally
/// tagged by `type`, with camelCase fields to match the API.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunTarget {
    /// Reuse an already-provisioned sandbox.
    Existing {
        #[serde(rename = "sandboxId")]
        sandbox_id: String,
    },
    /// Provision a fresh instance for the chosen GPU.
    New {
        gpu: String,
        #[serde(rename = "gpuCount")]
        gpu_count: i64,
        #[serde(rename = "diskGb")]
        disk_gb: i64,
    },
    /// Provision a fresh CPU-only instance.
    #[serde(rename = "new-cpu")]
    NewCpu {
        #[serde(rename = "cpuFlavor")]
        cpu_flavor: String,
        #[serde(rename = "vcpuCount")]
        vcpu_count: i64,
    },
}

#[derive(Debug, Clone, Serialize)]
struct RunBody {
    target: RunTarget,
}

// ---------------------------------------------------------------------------
// Core request helper — preserves TS error semantics exactly.
// ---------------------------------------------------------------------------

fn http() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(Client::new)
}

/// Sends a request and returns the response after applying the shared error
/// semantics (network failure, 401, other non-2xx). Body decoding is left to
/// the caller so both JSON-decoding and no-content endpoints can share this.
///
/// `body` is `None` for GET requests (no `content-type` header sent), or
/// `Some(json)` for a JSON request body, matching the TS `init` shape.
async fn send_request(
    creds: &Credentials,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<reqwest::Response> {
    let url = format!("{}{}", creds.api_url, path);
    let mut req = http().request(method, &url).bearer_auth(&creds.token);
    if let Some(ref b) = body {
        req = req.header("content-type", "application/json").json(b);
    }

    let res = match req.send().await {
        Ok(res) => res,
        Err(err) => {
            return Err(anyhow!(
                "Could not reach the API at {}: {}",
                creds.api_url,
                err
            ));
        }
    };

    let status = res.status();
    if status.as_u16() == 401 {
        return Err(anyhow!(
            "Unauthorized — your token is invalid or revoked. Run `orx login` again."
        ));
    }
    if !status.is_success() {
        let reason = status.canonical_reason().unwrap_or("");
        let detail = res.text().await.unwrap_or_default();
        let suffix = if detail.is_empty() {
            String::new()
        } else {
            format!(": {}", detail)
        };
        return Err(anyhow!(
            "Request to {} failed ({} {}){}",
            path,
            status.as_u16(),
            reason,
            suffix
        ));
    }

    Ok(res)
}

/// Issues a request and decodes the JSON body into `T`.
async fn request<T: DeserializeOwned>(
    creds: &Credentials,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<T> {
    let res = send_request(creds, method, path, body).await?;
    let parsed = res.json::<T>().await?;
    Ok(parsed)
}

/// Issues a request that returns no body (e.g. `204 No Content`).
async fn request_no_content(
    creds: &Credentials,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<()> {
    send_request(creds, method, path, body).await?;
    Ok(())
}

async fn api_get<T: DeserializeOwned>(creds: &Credentials, path: &str) -> Result<T> {
    request(creds, Method::GET, path, None).await
}

async fn api_post<T: DeserializeOwned>(creds: &Credentials, path: &str, body: Value) -> Result<T> {
    request(creds, Method::POST, path, Some(body)).await
}

async fn api_patch<T: DeserializeOwned>(creds: &Credentials, path: &str, body: Value) -> Result<T> {
    request(creds, Method::PATCH, path, Some(body)).await
}

// ---------------------------------------------------------------------------
// Endpoint fns (one per TS export, same path/method/shape)
// ---------------------------------------------------------------------------

pub async fn list_orgs(creds: &Credentials) -> Result<ListOrgs> {
    api_get(creds, "/orgs").await
}

pub async fn list_projects(creds: &Credentials, org_id: &str) -> Result<ListProjects> {
    api_get(creds, &format!("/orgs/{}/projects", org_id)).await
}

pub async fn list_experiments(creds: &Credentials, project_id: &str) -> Result<ListExperiments> {
    api_get(creds, &format!("/projects/{}/experiments", project_id)).await
}

pub async fn list_runs(creds: &Credentials, project_id: &str) -> Result<ListRuns> {
    api_get(creds, &format!("/projects/{}/runs", project_id)).await
}

pub async fn query_project(
    creds: &Credentials,
    project_id: &str,
    sql: &str,
) -> Result<ProjectQueryResult> {
    let body = serde_json::to_value(QueryBody { sql })?;
    api_post(creds, &format!("/projects/{}/query", project_id), body).await
}

pub async fn render_wandb_chart(
    creds: &Credentials,
    project_id: &str,
    body: &WandbChartBody,
) -> Result<WandbChartResult> {
    let body = serde_json::to_value(body)?;
    api_post(
        creds,
        &format!("/projects/{}/charts/wandb", project_id),
        body,
    )
    .await
}

pub async fn create_child_experiment(
    creds: &Credentials,
    project_id: &str,
    body: &CreateChildBody,
) -> Result<ExperimentEnvelope> {
    let body = serde_json::to_value(body)?;
    api_post(
        creds,
        &format!("/projects/{}/experiments", project_id),
        body,
    )
    .await
}

pub async fn create_empty_baseline(
    creds: &Credentials,
    project_id: &str,
    body: &CreateEmptyBaselineBody,
) -> Result<ExperimentEnvelope> {
    let body = serde_json::to_value(body)?;
    api_post(
        creds,
        &format!("/projects/{}/create-empty-baseline", project_id),
        body,
    )
    .await
}

pub async fn import_baseline(
    creds: &Credentials,
    project_id: &str,
    body: &ImportBaselineBody,
) -> Result<ExperimentEnvelope> {
    // Plain serde: `ref_` renames to `ref`, and `description: None` is omitted
    // (skip_serializing_if) to match the TS caller, which never sends the key.
    let json = serde_json::to_value(body)?;
    api_post(
        creds,
        &format!("/projects/{}/import-baseline", project_id),
        json,
    )
    .await
}

pub async fn dev_open(creds: &Credentials, exp_id: &str) -> Result<DevSession> {
    api_post(
        creds,
        &format!("/experiments/{}/dev/open", exp_id),
        serde_json::json!({}),
    )
    .await
}

pub async fn dev_status(creds: &Credentials, exp_id: &str) -> Result<DevStatus> {
    api_get(creds, &format!("/experiments/{}/dev/status", exp_id)).await
}

pub async fn dev_close(
    creds: &Credentials,
    exp_id: &str,
    body: &DevCloseBody,
) -> Result<DevCloseResult> {
    let body = serde_json::to_value(body)?;
    api_post(creds, &format!("/experiments/{}/dev/close", exp_id), body).await
}

pub async fn dev_fs(creds: &Credentials, exp_id: &str, op: &DevFsOp) -> Result<FsOutput> {
    let body = serde_json::to_value(op)?;
    api_post(creds, &format!("/experiments/{}/dev/fs", exp_id), body).await
}

pub async fn read_run_log(
    creds: &Credentials,
    run_id: &str,
    mode: Option<&str>,
    max_bytes: Option<i64>,
    start_byte: Option<i64>,
    end_byte: Option<i64>,
) -> Result<RunLogExcerpt> {
    let mut params: Vec<String> = Vec::new();
    if let Some(m) = mode {
        params.push(format!("mode={}", m));
    }
    if let Some(v) = max_bytes {
        params.push(format!("maxBytes={}", v));
    }
    if let Some(v) = start_byte {
        params.push(format!("startByte={}", v));
    }
    if let Some(v) = end_byte {
        params.push(format!("endByte={}", v));
    }
    let qs = if params.is_empty() {
        String::new()
    } else {
        format!("?{}", params.join("&"))
    };
    api_get(creds, &format!("/runs/{}/log{}", run_id, qs)).await
}

pub async fn search_logs(
    creds: &Credentials,
    project_id: &str,
    body: &SearchLogsBody,
) -> Result<LogSearchResult> {
    let body = serde_json::to_value(body)?;
    api_post(
        creds,
        &format!("/projects/{}/search-logs", project_id),
        body,
    )
    .await
}

pub async fn search_workdir(creds: &Credentials, exp_id: &str, query: &str) -> Result<FsOutput> {
    let q = urlencoding::encode(query);
    api_get(
        creds,
        &format!("/experiments/{}/workdir/search?q={}", exp_id, q),
    )
    .await
}

pub async fn ls_workdir(
    creds: &Credentials,
    exp_id: &str,
    path: Option<&str>,
) -> Result<WorkdirLs> {
    let qs = match path {
        Some(p) => format!("?path={}", urlencoding::encode(p)),
        None => String::new(),
    };
    api_get(creds, &format!("/experiments/{}/workdir/ls{}", exp_id, qs)).await
}

pub async fn read_workdir(creds: &Credentials, exp_id: &str, path: &str) -> Result<WorkdirRead> {
    let p = urlencoding::encode(path);
    api_get(
        creds,
        &format!("/experiments/{}/workdir/read?path={}", exp_id, p),
    )
    .await
}

pub async fn read_artifact(
    creds: &Credentials,
    run_id: &str,
    key: &str,
    mode: Option<&str>,
    max_bytes: Option<i64>,
) -> Result<ArtifactExcerpt> {
    let mut params: Vec<String> = vec![format!("key={}", urlencoding::encode(key))];
    if let Some(m) = mode {
        params.push(format!("mode={}", m));
    }
    if let Some(v) = max_bytes {
        params.push(format!("maxBytes={}", v));
    }
    api_get(
        creds,
        &format!("/runs/{}/artifact?{}", run_id, params.join("&")),
    )
    .await
}

pub async fn list_artifacts(creds: &Credentials, run_id: &str) -> Result<ListArtifacts> {
    api_get(creds, &format!("/runs/{}/artifacts", run_id)).await
}

pub async fn list_wandb_runs(creds: &Credentials, run_id: &str) -> Result<ListWandbRuns> {
    api_get(creds, &format!("/runs/{}/wandb-runs", run_id)).await
}

pub async fn get_run_diff(creds: &Credentials, run_id: &str) -> Result<RunDiff> {
    api_get(creds, &format!("/runs/{}/diff", run_id)).await
}

pub async fn list_catalog(creds: &Credentials) -> Result<ListCatalog> {
    api_get(creds, "/compute/catalog").await
}

pub async fn get_experiment(creds: &Credentials, exp_id: &str) -> Result<GetExperimentResult> {
    api_get(creds, &format!("/experiments/{}", exp_id)).await
}

pub async fn update_experiment(
    creds: &Credentials,
    exp_id: &str,
    body: &UpdateExperimentBody,
) -> Result<ExperimentEnvelope> {
    let body = serde_json::to_value(body)?;
    api_patch(creds, &format!("/experiments/{}", exp_id), body).await
}

pub async fn start_experiment_run(
    creds: &Credentials,
    exp_id: &str,
    target: RunTarget,
) -> Result<ExperimentEnvelope> {
    let body = serde_json::to_value(RunBody { target })?;
    api_post(creds, &format!("/experiments/{}/run", exp_id), body).await
}

pub async fn cancel_experiment_run(creds: &Credentials, exp_id: &str) -> Result<()> {
    request_no_content(
        creds,
        Method::POST,
        &format!("/experiments/{}/cancel", exp_id),
        Some(serde_json::json!({})),
    )
    .await
}

pub async fn list_skills(creds: &Credentials) -> Result<ListSkills> {
    api_get(creds, "/skills").await
}

pub async fn read_skill(creds: &Credentials, path: &str) -> Result<SkillContent> {
    let p = urlencoding::encode(path);
    api_get(creds, &format!("/skills/read?path={}", p)).await
}

// ---------------------------------------------------------------------------
// alphaXiv literature endpoints (public — no auth, different hosts).
//
// These do NOT go through `send_request`/`Credentials`: they hit alphaXiv's
// public API/web hosts and require no token, so `orx lit` / `orx paper` work
// even without `orx login`. They keep their own (simpler) error semantics and
// translate a 404 into `Ok(None)` where "not generated yet" is a normal answer.
// ---------------------------------------------------------------------------

/// Sent on external requests — some CDNs reject the default (empty) UA.
const ALPHAXIV_UA: &str = concat!("openresearch-cli/", env!("CARGO_PKG_VERSION"));

/// One full-text search hit (`GET /search/v2/paper/full-text`). Serialize is
/// derived so `orx lit --json` can re-emit hits verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaperHit {
    pub paper_id: String,
    pub title: String,
    #[serde(rename = "abstract", default)]
    pub abstract_: String,
    #[serde(default)]
    pub publication_date: Option<String>,
    #[serde(default)]
    pub votes: i64,
    #[serde(default)]
    pub snippets: Vec<PaperSnippet>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaperSnippet {
    #[serde(default)]
    pub page_number: i64,
    pub snippet: String,
}

/// Full-text literature search across alphaXiv. Returns the hits in relevance
/// order (most relevant first), capped at `limit`.
pub async fn search_papers(query: &str, limit: u32) -> Result<Vec<PaperHit>> {
    let base = crate::config::alphaxiv_api_url();
    let url = format!(
        "{}/search/v2/paper/full-text?q={}&limit={}",
        base,
        urlencoding::encode(query),
        limit
    );
    let res = http()
        .get(&url)
        .header("user-agent", ALPHAXIV_UA)
        .send()
        .await
        .map_err(|e| anyhow!("Could not reach alphaXiv at {}: {}", base, e))?;
    let status = res.status();
    if !status.is_success() {
        let reason = status.canonical_reason().unwrap_or("");
        return Err(anyhow!(
            "alphaXiv search failed ({} {})",
            status.as_u16(),
            reason
        ));
    }
    Ok(res.json::<Vec<PaperHit>>().await?)
}

/// Fetch one of a paper's markdown documents from the alphaXiv web app.
/// `kind` is `"overview"` (the machine-readable report) or `"abs"` (full text).
/// Returns `Ok(None)` on 404 — i.e. that document hasn't been generated yet.
pub async fn fetch_paper_markdown(kind: &str, paper_id: &str) -> Result<Option<String>> {
    let base = crate::config::alphaxiv_web_url();
    let url = format!("{}/{}/{}.md", base, kind, paper_id);
    let res = http()
        .get(&url)
        .header("user-agent", ALPHAXIV_UA)
        .send()
        .await
        .map_err(|e| anyhow!("Could not reach alphaXiv at {}: {}", base, e))?;
    let status = res.status();
    if status.as_u16() == 404 {
        return Ok(None);
    }
    if !status.is_success() {
        let reason = status.canonical_reason().unwrap_or("");
        return Err(anyhow!(
            "alphaXiv request for {} failed ({} {})",
            url,
            status.as_u16(),
            reason
        ));
    }
    Ok(Some(res.text().await?))
}
