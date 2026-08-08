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

use std::collections::HashMap;
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
    /// Populated from `launching_chat_session()`; None outside a chat session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_session_id: Option<String>,
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
    /// Populated from `launching_chat_session()`; None outside a chat session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_session_id: Option<String>,
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

/// `GET /sandboxes` — each row is a `Sandbox` (the extra `connections` the
/// dashboard renders is ignored on deserialize).
#[derive(Debug, Clone, Deserialize)]
pub struct ListSandboxes {
    pub sandboxes: Vec<Sandbox>,
}

/// A registered SSH public key (`zSshKey`, secrets-free). `public_key` is the
/// raw OpenSSH line, so the CLI can tell whether this machine holds the
/// matching private half.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshKey {
    pub id: String,
    pub name: String,
    pub public_key: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListSshKeys {
    pub ssh_keys: Vec<SshKey>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshKeyEnvelope {
    pub ssh_key: SshKey,
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

/// One box's provisioning state / SSH target — `GET /sandboxes/{id}`. The
/// openresearch backend polls this while its box goes provisioning → online.
pub async fn get_sandbox(creds: &Credentials, sandbox_id: &str) -> Result<SandboxEnvelope> {
    api_get(creds, &format!("/sandboxes/{}", sandbox_id)).await
}

/// Tear a box down (destroys the provider instance) — `DELETE /sandboxes/{id}`.
pub async fn delete_sandbox(creds: &Credentials, sandbox_id: &str) -> Result<()> {
    request_no_content(
        creds,
        Method::DELETE,
        &format!("/sandboxes/{}", sandbox_id),
        None,
    )
    .await
}

/// Every sandbox in an org (project-scoped + standalone) — `GET /sandboxes`.
pub async fn list_sandboxes(creds: &Credentials, org_id: &str) -> Result<ListSandboxes> {
    api_get(creds, &format!("/sandboxes?organizationId={}", org_id)).await
}

/// The user's registered SSH public keys — `GET /ssh-keys`. Boxes authorize
/// org members' registered keys, so an empty list means an unreachable box.
pub async fn list_ssh_keys(creds: &Credentials) -> Result<ListSshKeys> {
    api_get(creds, "/ssh-keys").await
}

/// Register a public key on the account — `POST /ssh-keys`. The api pushes it to
/// every live box in the user's orgs, so an already-running box becomes
/// reachable without a restart.
pub async fn create_ssh_key(
    creds: &Credentials,
    name: &str,
    public_key: &str,
) -> Result<SshKeyEnvelope> {
    api_post(
        creds,
        "/ssh-keys",
        serde_json::json!({ "name": name, "publicKey": public_key }),
    )
    .await
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

/// `2401.12345v2` → `2401.12345`; alphaXiv lookups want the versionless id.
fn versionless_id(paper_id: &str) -> &str {
    paper_id
        .rfind('v')
        .filter(|&i| i > 0 && !paper_id[i + 1..].is_empty())
        .filter(|&i| paper_id[i + 1..].chars().all(|c| c.is_ascii_digit()))
        .map_or(paper_id, |i| &paper_id[..i])
}

/// One hit from the fast (Google-backed) paper search — the endpoint built for
/// title lookups, vs the BM25 full-text search `orx lit` uses.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FastPaperHit {
    pub paper_id: String,
    pub title: String,
    #[serde(default)]
    pub snippet: Option<String>,
}

/// Title/keyword paper search (`GET /search/v2/paper/fast`).
pub async fn search_papers_fast(query: &str) -> Result<Vec<FastPaperHit>> {
    let base = crate::config::alphaxiv_api_url();
    let url = format!(
        "{}/search/v2/paper/fast?q={}&includePrivate=false",
        base,
        urlencoding::encode(query)
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
    Ok(res.json::<Vec<FastPaperHit>>().await?)
}

/// A paper resolved for the "start from a paper" project flow.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedPaper {
    /// Canonical versionless id (`2401.12345`).
    pub paper_id: String,
    pub title: Option<String>,
    /// Linked GitHub repo — author repos first, then most stars.
    pub repo_url: Option<String>,
    pub repo_stars: Option<i64>,
}

/// Resolve an arXiv id to title + linked GitHub repo. `/papers/v3/{id}` scrapes
/// arXiv on a miss, so brand-new papers resolve too (their repo links may lag).
/// The implementations lookup is best-effort — a failure there just means no repo.
pub async fn resolve_paper(paper_id: &str) -> Result<ResolvedPaper> {
    let id = versionless_id(paper_id);
    let base = crate::config::alphaxiv_api_url();
    let url = format!("{}/papers/v3/{}", base, urlencoding::encode(id));
    let res = http()
        .get(&url)
        .header("user-agent", ALPHAXIV_UA)
        .send()
        .await
        .map_err(|e| anyhow!("Could not reach alphaXiv at {}: {}", base, e))?;
    let status = res.status();
    if !status.is_success() {
        return Err(anyhow!(
            "Paper {} not found on alphaXiv/arXiv ({})",
            id,
            status.as_u16()
        ));
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Paper {
        group_id: Option<String>,
        universal_id: Option<String>,
        title: Option<String>,
    }
    let paper = res.json::<Paper>().await?;
    let mut resolved = ResolvedPaper {
        paper_id: paper.universal_id.unwrap_or_else(|| id.to_string()),
        title: paper.title,
        repo_url: None,
        repo_stars: None,
    };
    let Some(group_id) = paper.group_id.filter(|g| !g.is_empty()) else {
        return Ok(resolved);
    };

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Implementations {
        #[serde(default)]
        paper_resources: Vec<Resource>,
        #[serde(default)]
        alpha_xiv_implementations: Vec<Resource>,
    }
    #[derive(Deserialize)]
    struct Resource {
        #[serde(rename = "type")]
        kind: Option<String>,
        url: Option<String>,
        #[serde(default)]
        stars: Option<i64>,
        #[serde(default)]
        source: Option<String>,
    }
    let url = format!(
        "{}/papers/v3/{}/implementations",
        base,
        urlencoding::encode(&group_id)
    );
    let impls = match http()
        .get(&url)
        .header("user-agent", ALPHAXIV_UA)
        .send()
        .await
    {
        Ok(res) if res.status().is_success() => match res.json::<Implementations>().await {
            Ok(body) => body,
            Err(_) => return Ok(resolved),
        },
        _ => return Ok(resolved),
    };

    let is_github = |r: &&Resource| {
        r.kind.as_deref() == Some("github") && r.url.as_deref().is_some_and(|u| !u.is_empty())
    };
    let best = impls
        .paper_resources
        .iter()
        .filter(is_github)
        // Author repos beat community ones; stars break ties.
        .max_by_key(|r| (r.source.as_deref() == Some("author"), r.stars.unwrap_or(0)))
        .or_else(|| impls.alpha_xiv_implementations.iter().find(is_github));
    if let Some(repo) = best {
        resolved.repo_url = repo.url.clone();
        resolved.repo_stars = repo.stars;
    }
    Ok(resolved)
}

/// Look up a paper's linked GitHub repository (the most-starred repo associated
/// with it on alphaXiv). Returns `Ok(None)` when the paper has no linked repo or
/// isn't known to alphaXiv. Best-effort metadata — callers shouldn't fail on it.
pub async fn fetch_paper_github(paper_id: &str) -> Result<Option<String>> {
    // The feed lookup wants a versionless universal id (`2401.12345`, not `2401.12345v2`).
    let versionless = versionless_id(paper_id);
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

// ---------------------------------------------------------------------------
// Unified literature hit + OpenAlex / bioRxiv sources.
//
// `orx lit` searches one source per call and prints a uniform list; `orx paper`
// fetches one paper. Like the alphaXiv block above, these hit public hosts with
// no token and keep their own light error semantics. bioRxiv has no search API,
// so `--source biorxiv` searches OpenAlex filtered to bioRxiv's source and
// bioRxiv's own API is used only to fetch a preprint by DOI.
// ---------------------------------------------------------------------------

/// OpenAlex source id for the bioRxiv repository — `--source biorxiv` filters to it.
pub const BIORXIV_SOURCE_ID: &str = "S4306402567";

/// A single literature search hit, uniform across sources. `orx lit --json`
/// emits these verbatim, so per-source-only fields (`votes`, `citations`,
/// `snippets`) are omitted when empty.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LitHit {
    /// `"alphaxiv" | "openalex" | "biorxiv"` — set by the search fn.
    pub source: String,
    /// Self-routing id for `orx paper`: an arXiv id, a DOI, or an OpenAlex `W…` id.
    pub id: String,
    pub title: String,
    #[serde(rename = "abstract", default)]
    pub abstract_: String,
    #[serde(default)]
    pub publication_date: Option<String>,
    /// alphaXiv community votes; `None` for other sources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub votes: Option<i64>,
    /// Citation count (OpenAlex); `None` for alphaXiv.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub citations: Option<i64>,
    /// Matched full-text snippets; alphaXiv only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub snippets: Vec<PaperSnippet>,
}

impl From<PaperHit> for LitHit {
    fn from(h: PaperHit) -> Self {
        LitHit {
            source: "alphaxiv".to_string(),
            id: h.paper_id,
            title: h.title,
            abstract_: h.abstract_,
            publication_date: h.publication_date,
            votes: Some(h.votes),
            citations: None,
            snippets: h.snippets,
        }
    }
}

/// `https://openalex.org/W123` (or any `.../W123`) → `W123`.
fn strip_openalex_prefix(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

/// `https://doi.org/10.1101/x` / `doi:10.1101/x` → `10.1101/x`.
fn strip_doi_prefix(doi: &str) -> &str {
    doi.strip_prefix("https://doi.org/")
        .or_else(|| doi.strip_prefix("http://doi.org/"))
        .or_else(|| doi.strip_prefix("doi:"))
        .unwrap_or(doi)
}

/// Rebuild abstract text from OpenAlex's `abstract_inverted_index` (token →
/// positions). Returns `""` when the index is absent (OpenAlex omits abstracts
/// for some works).
fn reconstruct_abstract(index: &Option<HashMap<String, Vec<i64>>>) -> String {
    let Some(index) = index else {
        return String::new();
    };
    let mut positioned: Vec<(i64, &str)> = Vec::new();
    for (token, positions) in index {
        for &p in positions {
            positioned.push((p, token.as_str()));
        }
    }
    positioned.sort_by_key(|(p, _)| *p);
    positioned
        .into_iter()
        .map(|(_, t)| t)
        .collect::<Vec<_>>()
        .join(" ")
}

// OpenAlex serves snake_case JSON, so the Rust field names match the wire
// directly — no `rename_all` here (unlike the OpenResearch API DTOs above).
#[derive(Debug, Clone, Deserialize)]
struct OaAuthor {
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct OaAuthorship {
    #[serde(default)]
    author: Option<OaAuthor>,
}

#[derive(Debug, Clone, Deserialize)]
struct OaLocation {
    #[serde(default)]
    pdf_url: Option<String>,
    #[serde(default)]
    landing_page_url: Option<String>,
}

/// One OpenAlex work — used both for `/works` search rows (populated per the
/// `select` list) and for a single-work fetch (all fields present). Unselected
/// fields simply default.
#[derive(Debug, Clone, Deserialize)]
pub struct OpenAlexWork {
    // The three fields `paper.rs` reads straight off the struct stay `pub`; the
    // rest are reached through the accessor methods below.
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    doi: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub publication_date: Option<String>,
    #[serde(default)]
    pub cited_by_count: Option<i64>,
    #[serde(default)]
    abstract_inverted_index: Option<HashMap<String, Vec<i64>>>,
    #[serde(default)]
    authorships: Vec<OaAuthorship>,
    #[serde(default)]
    best_oa_location: Option<OaLocation>,
}

impl OpenAlexWork {
    /// Abstract text reconstructed from the inverted index (`""` when absent).
    pub fn abstract_text(&self) -> String {
        reconstruct_abstract(&self.abstract_inverted_index)
    }

    /// Author display names, in order.
    pub fn author_names(&self) -> Vec<String> {
        self.authorships
            .iter()
            .filter_map(|a| a.author.as_ref()?.display_name.clone())
            .collect()
    }

    /// Bare DOI (`10.…`) when the work has one.
    pub fn doi_bare(&self) -> Option<String> {
        self.doi.as_deref().map(|d| strip_doi_prefix(d).to_string())
    }

    /// Bare OpenAlex work id (`W…`) when present.
    pub fn work_id(&self) -> Option<String> {
        self.id
            .as_deref()
            .map(|i| strip_openalex_prefix(i).to_string())
    }

    /// A readable open-access URL: PDF if OpenAlex has one, else the landing page.
    pub fn oa_url(&self) -> Option<String> {
        self.best_oa_location
            .as_ref()
            .and_then(|l| l.pdf_url.clone().or_else(|| l.landing_page_url.clone()))
    }

    /// Best self-routing id for `orx paper`. For bioRxiv-sourced hits always the
    /// DOI (routes back to the richer bioRxiv fetch); otherwise the DOI when
    /// present, else the bare OpenAlex work id.
    fn routing_id(&self, prefer_doi: bool) -> String {
        let doi = self.doi_bare();
        if prefer_doi {
            if let Some(d) = doi {
                return d;
            }
        }
        doi.or_else(|| self.work_id()).unwrap_or_default()
    }

    fn into_lit_hit(self, biorxiv: bool) -> LitHit {
        let id = self.routing_id(biorxiv);
        let abstract_ = self.abstract_text();
        LitHit {
            source: if biorxiv { "biorxiv" } else { "openalex" }.to_string(),
            id,
            title: self.title.unwrap_or_default(),
            abstract_,
            publication_date: self.publication_date,
            votes: None,
            citations: self.cited_by_count,
            snippets: Vec::new(),
        }
    }
}

/// Fields to request from OpenAlex `/works` search — keeps the payload small.
const OPENALEX_SELECT: &str =
    "id,doi,title,publication_date,cited_by_count,abstract_inverted_index";

/// Search OpenAlex works by relevance, capped at `limit`. When `source_filter`
/// is set (an OpenAlex source id like [`BIORXIV_SOURCE_ID`]), results are
/// restricted to that venue. Hits come back already mapped to [`LitHit`].
pub async fn search_openalex(
    query: &str,
    limit: u32,
    source_filter: Option<&str>,
) -> Result<Vec<LitHit>> {
    let base = crate::config::openalex_api_url();
    // OpenAlex rejects per_page outside 1..=200 with a 400.
    let per_page = limit.clamp(1, 200);
    let mut url = format!(
        "{}/works?search={}&per_page={}&mailto={}&select={}",
        base,
        urlencoding::encode(query),
        per_page,
        urlencoding::encode(&crate::config::openalex_mailto()),
        OPENALEX_SELECT,
    );
    if let Some(sid) = source_filter {
        url.push_str("&filter=primary_location.source.id:");
        url.push_str(sid);
    }
    let res = http()
        .get(&url)
        .header("user-agent", ALPHAXIV_UA)
        .send()
        .await
        .map_err(|e| anyhow!("Could not reach OpenAlex at {}: {}", base, e))?;
    let status = res.status();
    if !status.is_success() {
        let reason = status.canonical_reason().unwrap_or("");
        return Err(anyhow!(
            "OpenAlex search failed ({} {})",
            status.as_u16(),
            reason
        ));
    }

    #[derive(Deserialize)]
    struct WorksResponse {
        #[serde(default)]
        results: Vec<OpenAlexWork>,
    }
    let biorxiv = source_filter == Some(BIORXIV_SOURCE_ID);
    let body = res.json::<WorksResponse>().await?;
    Ok(body
        .results
        .into_iter()
        .map(|w| w.into_lit_hit(biorxiv))
        .collect())
}

/// The `/works/{id}` selector for a work fetched by id or DOI. A DOI (bare,
/// `doi:`-prefixed, or a `doi.org` URL) becomes OpenAlex's `doi:<doi>` form
/// (slashes kept literal); anything else is treated as a bare `W…` work id.
fn openalex_selector(input: &str) -> String {
    let bare = strip_doi_prefix(input.trim());
    if bare.starts_with("10.") {
        return format!("doi:{}", bare);
    }
    strip_openalex_prefix(bare).to_string()
}

/// Fetch a single OpenAlex work by its `W…` id or a DOI. Returns `Ok(None)` on
/// 404 (unknown id) — a normal "not found" answer.
pub async fn fetch_openalex_work(id_or_doi: &str) -> Result<Option<OpenAlexWork>> {
    let base = crate::config::openalex_api_url();
    let url = format!(
        "{}/works/{}?mailto={}",
        base,
        openalex_selector(id_or_doi),
        urlencoding::encode(&crate::config::openalex_mailto()),
    );
    let res = http()
        .get(&url)
        .header("user-agent", ALPHAXIV_UA)
        .send()
        .await
        .map_err(|e| anyhow!("Could not reach OpenAlex at {}: {}", base, e))?;
    let status = res.status();
    if status.as_u16() == 404 {
        return Ok(None);
    }
    if !status.is_success() {
        let reason = status.canonical_reason().unwrap_or("");
        return Err(anyhow!(
            "OpenAlex lookup failed ({} {})",
            status.as_u16(),
            reason
        ));
    }
    Ok(Some(res.json::<OpenAlexWork>().await?))
}

/// One version row from the bioRxiv details API. `authors` is a single
/// semicolon-delimited string; `published` is the peer-reviewed DOI or the
/// literal string `"NA"`.
#[derive(Debug, Clone, Deserialize)]
pub struct BiorxivDetail {
    #[serde(default)]
    pub doi: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub authors: String,
    #[serde(rename = "abstract", default)]
    pub abstract_: String,
    #[serde(default)]
    pub date: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub published: String,
}

/// Fetch a bioRxiv preprint's metadata by DOI (`10.1101/…`). The details
/// endpoint lists every version oldest→newest; this returns the latest, or
/// `Ok(None)` when bioRxiv knows no such preprint (200 with an empty collection).
pub async fn fetch_biorxiv(doi: &str) -> Result<Option<BiorxivDetail>> {
    let base = crate::config::biorxiv_api_url();
    let url = format!("{}/details/biorxiv/{}/na/json", base, doi.trim());
    let res = http()
        .get(&url)
        .header("user-agent", ALPHAXIV_UA)
        .send()
        .await
        .map_err(|e| anyhow!("Could not reach bioRxiv at {}: {}", base, e))?;
    let status = res.status();
    if status.as_u16() == 404 {
        return Ok(None);
    }
    if !status.is_success() {
        let reason = status.canonical_reason().unwrap_or("");
        return Err(anyhow!(
            "bioRxiv lookup failed ({} {})",
            status.as_u16(),
            reason
        ));
    }

    #[derive(Deserialize)]
    struct DetailsResponse {
        #[serde(default)]
        collection: Vec<BiorxivDetail>,
    }
    let body = res.json::<DetailsResponse>().await?;
    Ok(body.collection.into_iter().last())
}

/// Search You.com web search API for current research and broader context.
/// Returns results mapped to [`LitHit`] format for consistency with other sources.
pub async fn search_youcom(query: &str, limit: u32) -> Result<Vec<LitHit>> {
    let api_key = std::env::var("YDC_API_KEY").ok();
    
    // Use keyless API if no API key is available
    let url = if api_key.is_some() {
        "https://api.you.com/v1/search"
    } else {
        "https://api.you.com/v1/agents/search"
    };
    
    let payload = if api_key.is_some() {
        serde_json::json!({
            "query": query,
            "count": limit
        })
    } else {
        serde_json::json!({
            "query": query,
            "count": limit
        })
    };
    
    let client = http();
    let mut req = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("User-Agent", ALPHAXIV_UA)
        .json(&payload);
    
    if let Some(key) = api_key {
        req = req.header("Authorization", format!("Bearer {}", key));
    }
    
    let res = req
        .send()
        .await
        .map_err(|e| anyhow!("Could not reach You.com API: {}", e))?;
    
    let status = res.status();
    if !status.is_success() {
        let error_text = res.text().await.unwrap_or_default();
        return Err(anyhow!(
            "You.com API error ({} {}): {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or(""),
            error_text
        ));
    }
    
    #[derive(serde::Deserialize)]
    struct YouComResult {
        title: String,
        url: String,
        #[serde(alias = "snippet")]
        description: String,
    }
    
    #[derive(serde::Deserialize, Default)]
    struct YouComWebResults {
        #[serde(default)]
        web: Vec<YouComResult>,
    }
    
    #[derive(serde::Deserialize)]
    struct YouComResponse {
        #[serde(default)]
        results: YouComWebResults,
        #[serde(default)]
        web: Vec<YouComResult>, // fallback for direct web results
    }
    
    let body = res.json::<YouComResponse>().await?;
    let results = if !body.results.web.is_empty() {
        body.results.web
    } else {
        body.web
    };
    
    Ok(results
        .into_iter()
        .map(|result| LitHit {
            source: "youcom".to_string(),
            id: result.url.clone(), // Use URL as id for web results
            title: result.title,
            abstract_: result.description,
            publication_date: None, // Web results don't have publication dates
            votes: None,
            citations: None,
            snippets: vec![], // No text snippets for web results
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::{
        openalex_selector, reconstruct_abstract, CreateBaselineExperimentBody, CreateChildBody,
        CreateSandboxBody, ListCatalog, ListCpuCatalog, LitHit, OpenAlexWork, PaperHit, RunBody,
        RunTarget, SandboxEnvelope, SandboxTarget, BIORXIV_SOURCE_ID,
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

    /// `GET /sandboxes/{id}` on an online box: ssh fields populated — this is
    /// the shape the openresearch backend's provisioning wait consumes. Extra
    /// keys (the list endpoint adds `connections`) must be tolerated.
    #[test]
    fn deserializes_sandbox_envelope_when_online() {
        let json = r#"{
            "sandbox": {
                "id": "sb_1",
                "organizationId": "org_1",
                "projectId": null,
                "sshHostname": "203.0.113.7",
                "sshPort": 22022,
                "sshUsername": "root",
                "status": "online",
                "machineType": "persistent",
                "createdBy": "user_1",
                "updatedAt": "2026-06-18T00:00:00Z",
                "provisionWarnings": null,
                "providerName": "runpod",
                "providerInstanceId": "pod-abc",
                "pricePerHour": 2.5,
                "gpu": "H100_SXM",
                "gpuCount": 1,
                "vcpuCount": null,
                "connections": []
            }
        }"#;

        let parsed: SandboxEnvelope = serde_json::from_str(json).expect("should deserialize");
        let sb = parsed.sandbox;
        assert_eq!(sb.status, "online");
        assert_eq!(sb.ssh_hostname.as_deref(), Some("203.0.113.7"));
        assert_eq!(sb.ssh_port, Some(22022));
        assert_eq!(sb.ssh_username.as_deref(), Some("root"));
    }

    /// The api declares `chatSessionId` optional: a lost `rename_all` would send
    /// `chat_session_id` and a lost `skip_serializing_if` would send `null`,
    /// either of which silently drops the row's attribution.
    #[test]
    fn serializes_experiment_chat_session_id() {
        let child = serde_json::to_value(CreateChildBody {
            title: "Child".into(),
            description: None,
            parent_experiment_id: "exp_parent".into(),
            chat_session_id: Some("ses_abc123".into()),
        })
        .unwrap();
        assert_eq!(child.get("chatSessionId"), Some(&json!("ses_abc123")));

        let child_without = serde_json::to_value(CreateChildBody {
            title: "Child".into(),
            description: None,
            parent_experiment_id: "exp_parent".into(),
            chat_session_id: None,
        })
        .unwrap();
        assert!(child_without.get("chatSessionId").is_none());

        let baseline = serde_json::to_value(CreateBaselineExperimentBody {
            title: Some("Baseline".into()),
            description: None,
            run_command: None,
            chat_session_id: Some("ses_abc123".into()),
        })
        .unwrap();
        assert_eq!(baseline.get("chatSessionId"), Some(&json!("ses_abc123")));

        let baseline_without = serde_json::to_value(CreateBaselineExperimentBody {
            title: Some("Baseline".into()),
            description: None,
            run_command: None,
            chat_session_id: None,
        })
        .unwrap();
        assert!(baseline_without.get("chatSessionId").is_none());
    }

    /// The inverted index maps token → positions; reconstruction must restore the
    /// original word order, and a missing index yields an empty string.
    #[test]
    fn reconstructs_openalex_abstract() {
        let work: OpenAlexWork = serde_json::from_str(
            r#"{ "abstract_inverted_index": { "Deep": [0], "learning": [1], "works": [2] } }"#,
        )
        .expect("should deserialize");
        assert_eq!(work.abstract_text(), "Deep learning works");
        assert_eq!(reconstruct_abstract(&None), "");
    }

    /// A DOI (bare, `doi:`-prefixed, or a `doi.org` URL) becomes the `doi:` form
    /// with slashes kept literal; anything else resolves to a bare `W…` work id.
    #[test]
    fn openalex_selector_routes_doi_vs_work_id() {
        assert_eq!(
            openalex_selector("10.1038/nature14539"),
            "doi:10.1038/nature14539"
        );
        assert_eq!(openalex_selector("doi:10.1038/x"), "doi:10.1038/x");
        assert_eq!(
            openalex_selector("https://doi.org/10.1101/2020.09.09.20191205"),
            "doi:10.1101/2020.09.09.20191205"
        );
        assert_eq!(openalex_selector("W2919115771"), "W2919115771");
        assert_eq!(
            openalex_selector("https://openalex.org/W2919115771"),
            "W2919115771"
        );
    }

    /// An OpenAlex `/works` row (id + doi as URLs, citations, inverted-index
    /// abstract, plus unselected extra fields) maps to a `LitHit`: DOI preferred
    /// as the routing id, `citations` set, `votes`/`snippets` empty.
    #[test]
    fn maps_openalex_work_to_lit_hit() {
        let work: OpenAlexWork = serde_json::from_str(
            r#"{
                "id": "https://openalex.org/W2919115771",
                "doi": "https://doi.org/10.1038/nature14539",
                "title": "Deep learning",
                "publication_date": "2015-05-26",
                "cited_by_count": 82932,
                "abstract_inverted_index": { "A": [0], "review.": [1] },
                "authorships": [{ "author": { "display_name": "Yann LeCun" } }],
                "some_unknown_field": true
            }"#,
        )
        .expect("should deserialize");

        assert_eq!(work.author_names(), vec!["Yann LeCun".to_string()]);
        assert_eq!(work.work_id().as_deref(), Some("W2919115771"));

        let hit = work.into_lit_hit(false);
        assert_eq!(hit.source, "openalex");
        assert_eq!(hit.id, "10.1038/nature14539");
        assert_eq!(hit.title, "Deep learning");
        assert_eq!(hit.abstract_, "A review.");
        assert_eq!(hit.citations, Some(82932));
        assert_eq!(hit.votes, None);
        assert!(hit.snippets.is_empty());
    }

    /// A bioRxiv-filtered OpenAlex hit routes through its `10.1101/…` DOI (so
    /// `orx paper` hits the richer bioRxiv fetch) and is labeled `biorxiv`.
    #[test]
    fn maps_biorxiv_filtered_hit_by_doi() {
        assert_eq!(BIORXIV_SOURCE_ID, "S4306402567");
        let work: OpenAlexWork = serde_json::from_str(
            r#"{
                "id": "https://openalex.org/W123",
                "doi": "https://doi.org/10.1101/2020.09.09.20191205",
                "title": "A preprint",
                "cited_by_count": 3
            }"#,
        )
        .expect("should deserialize");
        let hit = work.into_lit_hit(true);
        assert_eq!(hit.source, "biorxiv");
        assert_eq!(hit.id, "10.1101/2020.09.09.20191205");
        assert_eq!(hit.citations, Some(3));
    }

    /// An alphaXiv `PaperHit` maps to `LitHit` keeping `votes` and `snippets`;
    /// the JSON stays uniform, omitting the `None`/empty per-source fields.
    #[test]
    fn maps_paper_hit_and_serializes_uniform_json() {
        let ph: PaperHit = serde_json::from_str(
            r#"{
                "paperId": "2401.12345",
                "title": "A paper",
                "abstract": "Body.",
                "publicationDate": "2024-01-01T00:00:00Z",
                "votes": 7,
                "snippets": [{ "pageNumber": 2, "snippet": "hit" }]
            }"#,
        )
        .expect("should deserialize");

        let hit = LitHit::from(ph);
        assert_eq!(hit.source, "alphaxiv");
        assert_eq!(hit.id, "2401.12345");
        assert_eq!(hit.votes, Some(7));
        assert_eq!(hit.snippets.len(), 1);

        let value = serde_json::to_value(&hit).unwrap();
        assert_eq!(value.get("source"), Some(&json!("alphaxiv")));
        assert_eq!(value.get("votes"), Some(&json!(7)));
        // Per-source-only fields are omitted when empty.
        assert!(value.get("citations").is_none());

        let openalex = LitHit {
            source: "openalex".to_string(),
            id: "10.1/x".to_string(),
            title: "T".to_string(),
            abstract_: String::new(),
            publication_date: None,
            votes: None,
            citations: Some(5),
            snippets: Vec::new(),
        };
        let value = serde_json::to_value(&openalex).unwrap();
        assert_eq!(value.get("citations"), Some(&json!(5)));
        assert!(value.get("votes").is_none());
        assert!(value.get("snippets").is_none());
    }

    /// The bioRxiv details API wraps versions in a `collection`; we take the last
    /// (latest) and tolerate the extra top-level `messages` block.
    #[test]
    fn parses_biorxiv_latest_version() {
        #[derive(serde::Deserialize)]
        struct DetailsResponse {
            #[serde(default)]
            collection: Vec<super::BiorxivDetail>,
        }
        let body: DetailsResponse = serde_json::from_str(
            r#"{
                "messages": [{ "status": "ok" }],
                "collection": [
                    { "doi": "10.1101/2020.09.09.20191205", "title": "T", "version": "1",
                      "authors": "A; B", "abstract": "old", "date": "2020-09-09",
                      "category": "cell_biology", "published": "NA" },
                    { "doi": "10.1101/2020.09.09.20191205", "title": "T", "version": "2",
                      "authors": "A; B", "abstract": "new", "date": "2020-09-15",
                      "category": "cell_biology", "published": "10.1000/j.x" }
                ]
            }"#,
        )
        .expect("should deserialize");
        let latest = body.collection.into_iter().last().expect("has a version");
        assert_eq!(latest.version, "2");
        assert_eq!(latest.abstract_, "new");
        assert_eq!(latest.published, "10.1000/j.x");
    }
}
