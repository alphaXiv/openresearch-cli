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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: String,
    pub name: String,
    pub description: String,
    pub archived: bool,
    /// When true, anyone (incl. logged-out visitors) can view the project
    /// read-only. The `/projects/public` directory only returns these.
    #[serde(default)]
    pub is_public: bool,
    /// GitHub repo the project's experiment branches live on. Clone this to edit
    /// experiments locally: `git clone https://github.com/<owner>/<repo>.git`.
    #[serde(default)]
    pub github_owner: String,
    #[serde(default)]
    pub github_repo: String,
    /// One short, ready-to-send example question derived from the repo README.
    /// `None` until generated.
    #[serde(default)]
    pub example_question: Option<String>,
    /// Newest run in the project (UUIDv7 encodes the time), or `None` if no runs.
    #[serde(default)]
    pub last_activity_run_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Experiment {
    pub id: String,
    pub project_id: String,
    /// `null` for root experiments.
    pub parent_experiment_id: Option<String>,
    pub slug: String,
    /// The experiment's git branch on the project's GitHub repo (`orx/<slug>`).
    /// This is what you `git checkout` to edit the experiment's code.
    #[serde(default)]
    pub branch_name: String,
    pub title: String,
    /// Free-form notes / write-up for the experiment; empty string when unset.
    #[serde(default)]
    pub description: String,
    /// Optional analysis write-up; `null` when unset.
    #[serde(default)]
    pub analysis: Option<String>,
    pub run_command: String,
    /// `null` until the experiment has been linked to a sandbox.
    #[serde(default)]
    pub sandbox_id: Option<String>,
    /// The experiment agent's state, e.g. `"idle"` or `"implementing"`.
    #[serde(default)]
    pub agent_status: String,
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
    // Terminal time; only meaningful once `status` is terminal. Optional so
    // older API deployments (without the field) still deserialize.
    #[serde(default)]
    pub ended_at: Option<String>,
    // Seconds from run creation to end (or to now while still in-flight).
    #[serde(default)]
    pub duration_seconds: i64,
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
    pub region: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListCatalog {
    pub offers: Vec<GpuOffer>,
}

/// A single CPU-only offer from the CPU catalog (`GET /compute/catalog/cpu`).
/// Sibling to [`GpuOffer`]; CPU instances live in their own RunPod-only catalog.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CpuOffer {
    pub provider: String,
    pub offer_id: String,
    /// Flavor id: cpu5c (compute), cpu5g (general), or cpu5m (memory-optimized).
    pub cpu_flavor: String,
    /// Virtual CPUs allocated to the instance.
    pub vcpus: f64,
    /// System RAM in GB.
    pub ram_gb: f64,
    pub price_per_hour: f64,
    pub region: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListCpuCatalog {
    pub offers: Vec<CpuOffer>,
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

/// A research report attached to a project (`GET /projects/{id}/reports`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectReport {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub slug: String,
    pub created_at: String,
    pub created_by: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListReports {
    pub reports: Vec<ProjectReport>,
}

/// Response of `GET /projects/{id}/reports/{reportId}`: a report's metadata plus
/// its rendered markdown body (`report.md`). `markdown` is empty if the body was
/// never uploaded.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportDetail {
    pub report: ProjectReport,
    pub markdown: String,
}

/// One presigned upload slot returned by `POST /projects/{id}/reports`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportUploadSlot {
    pub path: String,
    pub url: String,
    pub content_type: String,
}

/// Response of `POST /projects/{id}/reports`: the created report plus the
/// presigned PUT URLs to upload each of its files directly to storage.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateReportResult {
    pub report: ProjectReport,
    pub uploads: Vec<ReportUploadSlot>,
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

/// A single environment variable the project's runs will see. Only the name and
/// where it's set are returned — values are never exposed over the CLI.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvVarName {
    pub key: String,
    /// `"org"`, `"project"`, or `"user"`.
    pub source: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListEnvVarNames {
    pub env_vars: Vec<EnvVarName>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListRuns {
    pub runs: Vec<Run>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExperimentEnvelope {
    pub experiment: Experiment,
}

/// Response of `POST /orgs/{orgId}/projects`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateProjectResult {
    pub is_first_project: bool,
    pub project: Project,
}

/// Response of `PATCH /projects/{id}`: the updated project row.
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectEnvelope {
    pub project: Project,
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
pub struct ImportBaselineBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Auto-generate suggested first experiments off the baseline. Defaults to
    /// true server-side when omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generate_suggestions: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateProjectBody {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `owner/repo` (or github.com URL) to bind the project to — the user's own
    /// repo, or a readable source it gets copied from. Omit to start the
    /// project on a fresh blank repo (a stub root commit on `main`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_full_name: Option<String>,
    /// Branch the baseline imports from (only with `repo_full_name`). Omit for
    /// the repo's default branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateReportBody {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    /// Report-relative paths to upload, e.g. ["report.md", "images/a.png"].
    pub files: Vec<String>,
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

/// PATCH body for `update_project`. Only the fields the CLI sets are included;
/// every field is optional and omitted when `None`.
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct UpdateProjectBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
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
    /// Bypass the server's "branch unchanged vs parent" guard. Omitted when false.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    force: bool,
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

/// The public project directory — every project flagged `isPublic`, viewable by
/// anyone. A PAT still works here but doesn't widen the result set.
pub async fn list_public_projects(creds: &Credentials) -> Result<ListProjects> {
    api_get(creds, "/projects/public").await
}

/// Fetch a single project by id (`GET /projects/{id}`). Works for any public
/// project, or any private one in an org the caller belongs to.
pub async fn get_project(creds: &Credentials, project_id: &str) -> Result<ProjectEnvelope> {
    api_get(creds, &format!("/projects/{}", project_id)).await
}

/// Find a project by id by scanning the caller's orgs. Prefer [`get_project`]
/// when you only need the row; this stays for callers that need org context.
pub async fn find_project(creds: &Credentials, project_id: &str) -> Result<Option<Project>> {
    for org in list_orgs(creds).await?.orgs {
        let found = list_projects(creds, &org.id)
            .await?
            .projects
            .into_iter()
            .find(|p| p.id == project_id);
        if found.is_some() {
            return Ok(found);
        }
    }
    Ok(None)
}

pub async fn create_project(
    creds: &Credentials,
    org_id: &str,
    body: &CreateProjectBody,
) -> Result<CreateProjectResult> {
    let body = serde_json::to_value(body)?;
    api_post(creds, &format!("/orgs/{}/projects", org_id), body).await
}

pub async fn update_project(
    creds: &Credentials,
    project_id: &str,
    body: &UpdateProjectBody,
) -> Result<ProjectEnvelope> {
    let body = serde_json::to_value(body)?;
    api_patch(creds, &format!("/projects/{}", project_id), body).await
}

pub async fn list_experiments(creds: &Credentials, project_id: &str) -> Result<ListExperiments> {
    api_get(creds, &format!("/projects/{}/experiments", project_id)).await
}

pub async fn list_env_var_names(creds: &Credentials, project_id: &str) -> Result<ListEnvVarNames> {
    api_get(creds, &format!("/projects/{}/env-var-names", project_id)).await
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

pub async fn import_baseline(
    creds: &Credentials,
    project_id: &str,
    body: &ImportBaselineBody,
) -> Result<ExperimentEnvelope> {
    // Repo is bound at project creation; this just materializes the baseline on
    // it. `None` fields are omitted so the server applies its defaults.
    let json = serde_json::to_value(body)?;
    api_post(
        creds,
        &format!("/projects/{}/import-baseline", project_id),
        json,
    )
    .await
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

pub async fn list_catalog(creds: &Credentials) -> Result<ListCatalog> {
    api_get(creds, "/compute/catalog").await
}

pub async fn list_cpu_catalog(creds: &Credentials) -> Result<ListCpuCatalog> {
    api_get(creds, "/compute/catalog/cpu").await
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
    force: bool,
) -> Result<ExperimentEnvelope> {
    let body = serde_json::to_value(RunBody { target, force })?;
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

pub async fn list_reports(creds: &Credentials, project_id: &str) -> Result<ListReports> {
    api_get(creds, &format!("/projects/{}/reports", project_id)).await
}

/// Fetch one report's metadata and its rendered markdown body.
pub async fn get_report(
    creds: &Credentials,
    project_id: &str,
    report_id: &str,
) -> Result<ReportDetail> {
    api_get(
        creds,
        &format!("/projects/{}/reports/{}", project_id, report_id),
    )
    .await
}

pub async fn create_report(
    creds: &Credentials,
    project_id: &str,
    body: &CreateReportBody,
) -> Result<CreateReportResult> {
    let body = serde_json::to_value(body)?;
    api_post(creds, &format!("/projects/{}/reports", project_id), body).await
}

/// Upload raw bytes to a presigned PUT URL (R2). No auth header — the signature
/// in the URL authorizes the write. `content_type` must match what the server
/// signed (the value returned alongside the URL).
pub async fn upload_to_presigned(url: &str, content_type: &str, bytes: Vec<u8>) -> Result<()> {
    let res = http()
        .put(url)
        .header("content-type", content_type)
        .body(bytes)
        .send()
        .await
        .map_err(|e| anyhow!("Could not upload to storage: {}", e))?;
    let status = res.status();
    if !status.is_success() {
        let reason = status.canonical_reason().unwrap_or("");
        return Err(anyhow!("Upload failed ({} {})", status.as_u16(), reason));
    }
    Ok(())
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

/// Look up a paper's linked GitHub repository (the most-starred repo associated
/// with it on alphaXiv). Returns `Ok(None)` when the paper has no linked repo or
/// isn't known to alphaXiv. Best-effort metadata — callers shouldn't fail on it.
pub async fn fetch_paper_github(paper_id: &str) -> Result<Option<String>> {
    // The feed lookup wants a versionless universal id (`2401.12345`, not `2401.12345v2`).
    let versionless = paper_id
        .rfind('v')
        .filter(|&i| i > 0 && !paper_id[i + 1..].is_empty())
        .filter(|&i| paper_id[i + 1..].chars().all(|c| c.is_ascii_digit()))
        .map_or(paper_id, |i| &paper_id[..i]);
    let base = crate::config::alphaxiv_api_url();
    let url = format!(
        "{}/papers/v3/feed?universalId={}&pageNum=0&pageSize=1&sort=Hot&interval=All%20time",
        base,
        urlencoding::encode(versionless)
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
            "alphaXiv paper lookup failed ({} {})",
            status.as_u16(),
            reason
        ));
    }

    #[derive(Deserialize)]
    struct FeedResponse {
        papers: Vec<FeedPaper>,
    }
    #[derive(Deserialize)]
    struct FeedPaper {
        github_url: Option<String>,
    }

    let body = res.json::<FeedResponse>().await?;
    Ok(body.papers.into_iter().next().and_then(|p| p.github_url))
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
