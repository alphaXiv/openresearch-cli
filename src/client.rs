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
    /// arXiv id of the paper this project reproduces, derived from the repo
    /// README at creation. `None` when the repo names no paper. This is the
    /// key the publish-to-alphaXiv sweep matches a finished report against.
    #[serde(default)]
    pub paper_id: Option<String>,
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
    // The compute the run executed on. Optional so older API deployments (which
    // omit the field) still deserialize.
    #[serde(default)]
    pub sandbox_id: Option<String>,
    // Object-storage key for the run's logs, once captured. Where to look for the
    // "why" when a run fails *after* the box is up (e.g. the script exited
    // non-zero) and `result_markdown` is therefore empty.
    #[serde(default)]
    pub log_key: Option<String>,
    // Human-readable terminal detail. On failure during compute spin-up this
    // holds the provider error the website shows as a toast (e.g. "Provisioning
    // failed: RunPod … Out of capacity"); on a successful run it's the run's
    // EVAL.md. Null for runtime failures after the box came up — see `log_key`.
    #[serde(default)]
    pub result_markdown: Option<String>,
    // Terminal time; only meaningful once `status` is terminal. Optional so
    // older API deployments (without the field) still deserialize.
    #[serde(default)]
    pub ended_at: Option<String>,
    // Seconds from run creation to end (or to now while still in-flight).
    #[serde(default)]
    pub duration_seconds: i64,
}

/// Disk pricing for an offer. Mirrors the backend `zDisk` discriminated union,
/// keyed on the `sizable` bool: when `true`, `per_gb_hour` is set and the disk
/// bills per GB/hour; when `false`, `included_gb` is set and the offer bundles a
/// fixed capacity. Modeled as a flat struct with optional payloads rather than an
/// enum because serde's tagged enums can't key on a bool discriminator, and an
/// untagged enum would not apply the container's `camelCase` rename to variant
/// fields. The unused payload is simply `None`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Disk {
    pub sizable: bool,
    pub per_gb_hour: Option<f64>,
    pub included_gb: Option<f64>,
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
    pub disk: Disk,
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
    pub disk: Disk,
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
pub struct CreateBaselineExperimentBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Run command seeded onto the baseline so it's launchable immediately.
    /// Omit to set it later (`orx exp cmd`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_command: Option<String>,
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
    /// Branch of the repo the project binds to (only with `repo_full_name`) —
    /// the baseline experiment branches off it. Omit for the repo's default.
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
    /// Project visibility (`isPublic`): `Some(true)` lists it in the public
    /// directory, `Some(false)` makes it private. `None` leaves it unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_public: Option<bool>,
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
        /// Single lowercase word — same under camelCase, so no rename needed.
        /// Omitted from the payload when `None`.
        #[serde(skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
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

/// The `target` of a standalone instance (`POST /sandboxes`). Mirrors
/// `RunTarget`'s `New`/`NewCpu` variants, minus `Existing` — a standalone box is
/// always freshly provisioned, never an existing-sandbox reuse. Kept separate
/// from `RunTarget` because the two hit different endpoints whose contracts may
/// diverge.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SandboxTarget {
    /// Provision a fresh GPU instance.
    New {
        gpu: String,
        #[serde(rename = "gpuCount")]
        gpu_count: i64,
        #[serde(rename = "diskGb")]
        disk_gb: i64,
        /// Single lowercase word — same under camelCase, so no rename needed.
        /// Omitted from the payload when `None`.
        #[serde(skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
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

/// Body of `POST /sandboxes`. `projectId` is intentionally omitted — the server
/// rejects it for `new`/`new-cpu` (those are org-level standalone only).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateSandboxBody {
    pub organization_id: String,
    pub target: SandboxTarget,
}

/// A sandbox as returned by `POST /sandboxes`. Mirrors the API's `zSandbox`;
/// fields are nullable while a hosted box is still provisioning.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sandbox {
    pub id: String,
    pub organization_id: String,
    pub project_id: Option<String>,
    pub ssh_hostname: Option<String>,
    pub ssh_port: Option<i64>,
    pub ssh_username: Option<String>,
    pub status: String,
    pub machine_type: String,
    pub created_by: Option<String>,
    pub updated_at: String,
    pub provision_warnings: Option<String>,
    pub provider_name: Option<String>,
    pub provider_instance_id: Option<String>,
    pub price_per_hour: Option<f64>,
    pub gpu: Option<String>,
    pub gpu_count: Option<i64>,
    pub vcpu_count: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SandboxEnvelope {
    pub sandbox: Sandbox,
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

pub async fn create_baseline_experiment(
    creds: &Credentials,
    project_id: &str,
    body: &CreateBaselineExperimentBody,
) -> Result<ExperimentEnvelope> {
    // Repo is bound at project creation; this materializes a baseline (root
    // node) on it. `None` fields are omitted so the server applies its
    // defaults. Repeat calls create additional roots — projects may hold
    // multiple baselines.
    let json = serde_json::to_value(body)?;
    api_post(
        creds,
        &format!("/projects/{}/baseline-experiment", project_id),
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

/// Spin up a standalone instance in an org (no experiment) — `POST /sandboxes`.
pub async fn create_sandbox(
    creds: &Credentials,
    body: &CreateSandboxBody,
) -> Result<SandboxEnvelope> {
    let body = serde_json::to_value(body)?;
    api_post(creds, "/sandboxes", body).await
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

/// Download the raw bytes of one file within a report (e.g. `report.md` or an
/// image referenced from it). The endpoint 302-redirects to a presigned R2 URL;
/// `reqwest` follows it (and drops the bearer header on the cross-host hop, which
/// is correct — the signature in the URL authorizes the read). `path` is a
/// report-relative POSIX path like `images/loss.png`.
pub async fn download_report_file(
    creds: &Credentials,
    project_id: &str,
    report_id: &str,
    path: &str,
) -> Result<Vec<u8>> {
    let encoded = urlencoding::encode(path);
    let res = send_request(
        creds,
        Method::GET,
        &format!(
            "/projects/{}/reports/{}/file?path={}",
            project_id, report_id, encoded
        ),
        None,
    )
    .await?;
    let bytes = res
        .bytes()
        .await
        .map_err(|e| anyhow!("Could not read {}: {}", path, e))?;
    Ok(bytes.to_vec())
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

// ---------------------------------------------------------------------------
// External runs (jobs executed by orx itself — HF Jobs etc.). The api is a
// mirror: create registers the row, PATCH reports transitions (and returns
// cancel intent), the log presign hands back a PUT URL for the final log.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalRunLite {
    pub id: String,
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalRunCreated {
    pub run: ExternalRunLite,
    pub project_id: String,
    pub run_command: String,
    pub branch_name: String,
    pub github_owner: String,
    pub github_repo: String,
    /// Short-lived repo-scoped read token from the org's connected GitHub app,
    /// for the job's private-repo clone. Null for mint failures.
    #[serde(default)]
    pub github_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalRunPatched {
    pub cancel_requested: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalRunState {
    pub status: String,
    pub cancel_requested: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PresignedUrl {
    pub url: String,
}

pub async fn create_external_run(
    creds: &Credentials,
    exp_id: &str,
    backend: Value,
) -> Result<ExternalRunCreated> {
    api_post(
        creds,
        &format!("/experiments/{}/external-run", exp_id),
        serde_json::json!({ "backend": backend }),
    )
    .await
}

/// Report a transition and/or descriptor update. Fields are all optional; the
/// response's `cancelRequested` doubles as the supervisor's cancel poll.
pub async fn update_external_run(
    creds: &Credentials,
    run_id: &str,
    body: Value,
) -> Result<ExternalRunPatched> {
    api_patch(creds, &format!("/runs/{}/external", run_id), body).await
}

pub async fn get_external_run_state(creds: &Credentials, run_id: &str) -> Result<ExternalRunState> {
    api_get(creds, &format!("/runs/{}/external", run_id)).await
}

pub async fn presign_external_run_log(creds: &Credentials, run_id: &str) -> Result<PresignedUrl> {
    api_post(
        creds,
        &format!("/runs/{}/external-log", run_id),
        serde_json::json!({}),
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

#[cfg(test)]
mod tests {
    use super::{
        CreateSandboxBody, ListCatalog, ListCpuCatalog, RunBody, RunTarget, SandboxEnvelope,
        SandboxTarget,
    };
    use serde_json::json;

    /// The GPU catalog wire format carries `disk` as a discriminated union and an
    /// optional `region`, plus `bandwidth*` fields the CLI ignores. Pin that we
    /// decode both disk shapes, treat a missing region as `None`, and tolerate the
    /// extra fields — this is exactly the drift that previously broke `orx compute`.
    #[test]
    fn deserializes_gpu_catalog_with_disk_union_and_optional_region() {
        let json = r#"{
            "offers": [
                {
                    "provider": "runpod",
                    "offerId": "a",
                    "gpu": "H100_SXM",
                    "gpuCount": 1,
                    "vcpus": 16,
                    "ramGb": 188,
                    "pricePerHour": 2.5,
                    "disk": { "sizable": true, "perGbHour": 0.0001 },
                    "bandwidthInPerGb": 0,
                    "bandwidthOutPerGb": 0,
                    "region": "US_CA"
                },
                {
                    "provider": "lambda",
                    "offerId": "b",
                    "gpu": "A100_SXM_80GB",
                    "gpuCount": 8,
                    "vcpus": 124,
                    "ramGb": 1800,
                    "pricePerHour": 14.0,
                    "disk": { "sizable": false, "includedGb": 1024 },
                    "bandwidthInPerGb": 0,
                    "bandwidthOutPerGb": 0
                }
            ]
        }"#;

        let parsed: ListCatalog = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(parsed.offers.len(), 2);

        let sizable = &parsed.offers[0];
        assert_eq!(sizable.region.as_deref(), Some("US_CA"));
        assert!(sizable.disk.sizable);
        assert_eq!(sizable.disk.per_gb_hour, Some(0.0001));
        assert_eq!(sizable.disk.included_gb, None);

        let fixed = &parsed.offers[1];
        // `region` absent on the wire must decode to `None`.
        assert_eq!(fixed.region, None);
        assert!(!fixed.disk.sizable);
        assert_eq!(fixed.disk.included_gb, Some(1024.0));
        assert_eq!(fixed.disk.per_gb_hour, None);
    }

    /// CPU offers share the same `disk` union; pin that the CPU catalog decodes too.
    #[test]
    fn deserializes_cpu_catalog_with_disk_union() {
        let json = r#"{
            "offers": [
                {
                    "provider": "runpod",
                    "offerId": "c",
                    "cpuFlavor": "cpu5c",
                    "vcpus": 4,
                    "ramGb": 16,
                    "pricePerHour": 0.1,
                    "disk": { "sizable": true, "perGbHour": 0.0001 }
                }
            ]
        }"#;

        let parsed: ListCpuCatalog = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(parsed.offers.len(), 1);
        assert!(parsed.offers[0].disk.sizable);
        assert_eq!(parsed.offers[0].disk.per_gb_hour, Some(0.0001));
    }

    /// The `new` GPU run target serializes with the discriminant and camelCase
    /// keys the API expects, including `provider` when set.
    #[test]
    fn serializes_run_target_new_with_provider() {
        let target = RunTarget::New {
            gpu: "H100_SXM".into(),
            gpu_count: 1,
            disk_gb: 100,
            provider: Some("runpod".into()),
        };
        assert_eq!(
            serde_json::to_value(&target).unwrap(),
            json!({"type": "new", "gpu": "H100_SXM", "gpuCount": 1, "diskGb": 100, "provider": "runpod"}),
        );
    }

    /// A `None` provider must be omitted from the payload entirely (so the server
    /// falls back to its own default), not sent as `null`.
    #[test]
    fn serializes_run_target_new_without_provider() {
        let target = RunTarget::New {
            gpu: "H100_SXM".into(),
            gpu_count: 2,
            disk_gb: 200,
            provider: None,
        };
        let value = serde_json::to_value(&target).unwrap();
        assert_eq!(
            value,
            json!({"type": "new", "gpu": "H100_SXM", "gpuCount": 2, "diskGb": 200}),
        );
        assert!(value.get("provider").is_none());
    }

    /// `force` is omitted when false and present when true.
    #[test]
    fn serializes_run_body_force_flag() {
        let target = RunTarget::New {
            gpu: "H100_SXM".into(),
            gpu_count: 1,
            disk_gb: 100,
            provider: Some("vast".into()),
        };
        let with_force = serde_json::to_value(RunBody {
            target: target.clone(),
            force: true,
        })
        .unwrap();
        assert_eq!(with_force.get("force"), Some(&json!(true)));

        let without_force = serde_json::to_value(RunBody {
            target,
            force: false,
        })
        .unwrap();
        assert!(without_force.get("force").is_none());
    }

    /// The standalone GPU sandbox target mirrors the run target's wire shape.
    #[test]
    fn serializes_sandbox_target_new() {
        let target = SandboxTarget::New {
            gpu: "H100_SXM".into(),
            gpu_count: 2,
            disk_gb: 100,
            provider: Some("vast".into()),
        };
        assert_eq!(
            serde_json::to_value(&target).unwrap(),
            json!({"type": "new", "gpu": "H100_SXM", "gpuCount": 2, "diskGb": 100, "provider": "vast"}),
        );
    }

    /// Omitting the provider must drop the key entirely — that's what lets the
    /// server pick the cheapest offer across providers for `instance create`.
    #[test]
    fn serializes_sandbox_target_new_without_provider() {
        let target = SandboxTarget::New {
            gpu: "H100_SXM".into(),
            gpu_count: 1,
            disk_gb: 100,
            provider: None,
        };
        let value = serde_json::to_value(&target).unwrap();
        assert_eq!(
            value,
            json!({"type": "new", "gpu": "H100_SXM", "gpuCount": 1, "diskGb": 100}),
        );
        assert!(value.get("provider").is_none());
    }

    /// The CPU sandbox target uses the `new-cpu` discriminant and camelCase keys.
    #[test]
    fn serializes_sandbox_target_new_cpu() {
        let target = SandboxTarget::NewCpu {
            cpu_flavor: "cpu5g".into(),
            vcpu_count: 8,
        };
        assert_eq!(
            serde_json::to_value(&target).unwrap(),
            json!({"type": "new-cpu", "cpuFlavor": "cpu5g", "vcpuCount": 8}),
        );
    }

    /// The create-sandbox body sends `organizationId` and never a `projectId`
    /// (the server rejects a project-scoped `new`/`new-cpu`).
    #[test]
    fn serializes_create_sandbox_body_without_project() {
        let body = CreateSandboxBody {
            organization_id: "org_123".into(),
            target: SandboxTarget::NewCpu {
                cpu_flavor: "cpu5c".into(),
                vcpu_count: 2,
            },
        };
        let value = serde_json::to_value(&body).unwrap();
        assert_eq!(value.get("organizationId"), Some(&json!("org_123")));
        assert!(value.get("projectId").is_none());
    }

    /// `POST /sandboxes` returns a freshly-provisioning box: ssh fields are still
    /// `null` while the GPU/provider/price fields are already populated from the
    /// offer. Pin that we decode that shape (camelCase keys, nulls → `None`).
    #[test]
    fn deserializes_sandbox_envelope_while_provisioning() {
        let json = r#"{
            "sandbox": {
                "id": "sb_1",
                "organizationId": "org_1",
                "projectId": null,
                "sshHostname": null,
                "sshPort": null,
                "sshUsername": null,
                "status": "provisioning",
                "machineType": "persistent",
                "createdBy": "user_1",
                "updatedAt": "2026-06-18T00:00:00Z",
                "provisionWarnings": null,
                "providerName": "runpod",
                "providerInstanceId": null,
                "pricePerHour": 2.5,
                "gpu": "H100_SXM",
                "gpuCount": 1,
                "vcpuCount": null
            }
        }"#;

        let parsed: SandboxEnvelope = serde_json::from_str(json).expect("should deserialize");
        let sb = parsed.sandbox;
        assert_eq!(sb.id, "sb_1");
        assert_eq!(sb.status, "provisioning");
        assert_eq!(sb.project_id, None);
        assert_eq!(sb.ssh_hostname, None);
        assert_eq!(sb.provider_name.as_deref(), Some("runpod"));
        assert_eq!(sb.gpu.as_deref(), Some("H100_SXM"));
        assert_eq!(sb.gpu_count, Some(1));
        assert_eq!(sb.vcpu_count, None);
        assert_eq!(sb.price_per_hour, Some(2.5));
    }
}
