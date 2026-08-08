//! Ray Jobs client — REST surface for `orx exp run --backend ray`.
//!
//! Talks to the Ray 2.x Jobs API, which the cluster's Dashboard serves (hence
//! the dashboard port in the default address). Every function here expects a
//! `resolve_address`-normalized base URL (trimmed, no trailing slash).

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{anyhow, Result};

const DEFAULT_ADDRESS: &str = "http://127.0.0.1:8265";

// --- settings ---------------------------------------------------------------

/// User-tunable Ray Jobs defaults at `$XDG_CONFIG_HOME/openresearch/ray.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RaySettings {
    /// Jobs / Dashboard base URL as the user typed it; normalized on read by
    /// `resolve_address`.
    #[serde(default)]
    pub address: Option<String>,
}

fn settings_path() -> std::path::PathBuf {
    crate::config::config_dir().join("ray.json")
}

pub fn load_settings() -> Result<Option<RaySettings>> {
    let raw = match std::fs::read_to_string(settings_path()) {
        Ok(raw) => raw,
        Err(_) => return Ok(None),
    };
    match serde_json::from_str::<RaySettings>(&raw) {
        Ok(s) => Ok(Some(s)),
        Err(e) => Err(anyhow!(
            "Unreadable {} ({}). Fix or delete it and reconfigure.",
            settings_path().display(),
            e
        )),
    }
}

pub fn save_settings(settings: &RaySettings) -> Result<()> {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = format!("{}\n", serde_json::to_string_pretty(settings)?);
    std::fs::write(&path, body)?;
    Ok(())
}

/// Resolve the Jobs API base URL (trimmed, no trailing slash), preferring an
/// explicit caller-supplied address over the saved/env/default chain.
pub fn resolve_address(explicit: Option<&str>) -> String {
    match explicit.map(str::trim).filter(|s| !s.is_empty()) {
        Some(a) => a.trim_end_matches('/').to_string(),
        None => resolve_address_with_source().0,
    }
}

/// Where the address came from (settings UI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressSource {
    Settings,
    AstroaiEnv,
    RayEnv,
    Default,
}

pub fn resolve_address_with_source() -> (String, AddressSource) {
    let clean = |a: String| a.trim().trim_end_matches('/').to_string();
    let nonempty = |a: Option<String>| a.filter(|s| !s.trim().is_empty());
    if let Some(a) = nonempty(load_settings().ok().flatten().and_then(|s| s.address)) {
        return (clean(a), AddressSource::Settings);
    }
    if let Some(a) = nonempty(std::env::var("ASTROAI_RAY_JOBS_ADDRESS").ok()) {
        return (clean(a), AddressSource::AstroaiEnv);
    }
    if let Some(a) = nonempty(std::env::var("RAY_DASHBOARD_URL").ok()) {
        return (clean(a), AddressSource::RayEnv);
    }
    (DEFAULT_ADDRESS.to_string(), AddressSource::Default)
}

// --- flavor → resources -----------------------------------------------------

#[derive(Debug, Clone)]
pub struct RayResources {
    pub cpus: f64,
    pub gpus: f64,
    pub memory_bytes: Option<u64>,
}

impl Default for RayResources {
    fn default() -> Self {
        // cpus=0: do not reserve entrypoint CPUs (avoids Pending on small heads).
        Self {
            cpus: 0.0,
            gpus: 0.0,
            memory_bytes: None,
        }
    }
}

/// Parse an optional flavor into Ray entrypoint resources.
///
/// Examples: `cpu`, `cpu:2`, `gpu`, `gpu:1`, `gpu:1,cpu:4`, `gpu:1,mem:8GiB`.
pub fn parse_flavor(flavor: Option<&str>) -> Result<RayResources> {
    let Some(raw) = flavor.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(RayResources::default());
    };
    let mut out = RayResources::default();
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (key, val) = match part.split_once(':') {
            Some((k, v)) => (k.trim().to_ascii_lowercase(), Some(v.trim())),
            None => (part.to_ascii_lowercase(), None),
        };
        match key.as_str() {
            "cpu" | "cpus" => {
                out.cpus = match val {
                    None | Some("") => 1.0,
                    Some(v) => v.parse::<f64>().map_err(|_| {
                        anyhow!("Invalid Ray flavor CPU count in {raw:?} (got {v:?})")
                    })?,
                };
            }
            "gpu" | "gpus" => {
                out.gpus = match val {
                    None | Some("") => 1.0,
                    Some(v) => v.parse::<f64>().map_err(|_| {
                        anyhow!("Invalid Ray flavor GPU count in {raw:?} (got {v:?})")
                    })?,
                };
            }
            "mem" | "memory" => {
                let v = val.ok_or_else(|| {
                    anyhow!("Ray flavor memory needs a size, e.g. mem:8GiB (got {raw:?})")
                })?;
                out.memory_bytes = Some(parse_memory(v)?);
            }
            other => {
                return Err(anyhow!(
                    "Unknown Ray flavor token {other:?} in {raw:?}. \
                     Use cpu[:N], gpu[:N], and/or mem:<size> (e.g. gpu:1,cpu:4,mem:8GiB)."
                ));
            }
        }
    }
    if !(out.cpus.is_finite() && out.cpus >= 0.0 && out.gpus.is_finite() && out.gpus >= 0.0) {
        return Err(anyhow!("Ray flavor cpus/gpus must be non-negative numbers"));
    }
    Ok(out)
}

fn parse_memory(value: &str) -> Result<u64> {
    let value = value.trim();
    let digits: String = value
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let unit: String = value[digits.len()..].trim().to_ascii_lowercase();
    let amount: f64 = digits
        .parse()
        .map_err(|_| anyhow!("Invalid memory size {value:?}"))?;
    let factor: f64 = match unit.as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1000.0,
        "ki" | "kib" => 1024.0,
        "m" | "mb" => 1000f64.powi(2),
        "mi" | "mib" => 1024f64.powi(2),
        "g" | "gb" => 1000f64.powi(3),
        "gi" | "gib" => 1024f64.powi(3),
        "t" | "tb" => 1000f64.powi(4),
        "ti" | "tib" => 1024f64.powi(4),
        _ => {
            return Err(anyhow!(
                "Unknown memory unit in {value:?} (try 8GiB or 512MiB)"
            ))
        }
    };
    let nbytes = (amount * factor) as u64;
    if nbytes == 0 {
        return Err(anyhow!("memory must be positive"));
    }
    Ok(nbytes)
}

// --- HTTP --------------------------------------------------------------------

fn http() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .build()
            .expect("reqwest client")
    })
}

async fn check(res: reqwest::Response, what: &str) -> Result<reqwest::Response> {
    let status = res.status();
    if status.is_success() {
        return Ok(res);
    }
    let body = res.text().await.unwrap_or_default();
    Err(anyhow!(
        "Ray Jobs {} failed ({}): {}",
        what,
        status.as_u16(),
        body
    ))
}

/// Probe the Jobs API; `Ok` means reachable, carrying the cluster's Ray
/// version when it reports one.
pub async fn preflight(address: &str) -> Result<Option<String>> {
    let res = http()
        .get(format!("{address}/api/version"))
        .send()
        .await
        .map_err(|e| anyhow!("Could not reach Ray Jobs at {address}: {e}"))?;
    let res = check(res, "preflight").await?;
    let body: serde_json::Value = res.json().await.unwrap_or(json!({}));
    Ok(body
        .get("ray_version")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string()))
}

pub struct JobSubmission {
    pub entrypoint: String,
    pub submission_id: String,
    pub resources: RayResources,
    pub env: HashMap<String, String>,
    pub metadata: HashMap<String, String>,
    pub working_dir: Option<String>,
}

#[derive(Debug, Clone)]
pub struct JobInfo {
    /// Shared stage vocabulary (`SCHEDULING` / `RUNNING` / `COMPLETED` / …).
    pub stage: String,
    pub message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawJobStatus {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

fn map_ray_status(raw: &str) -> String {
    match raw.to_ascii_uppercase().as_str() {
        "PENDING" => "SCHEDULING".into(),
        "RUNNING" => "RUNNING".into(),
        "SUCCEEDED" | "COMPLETED" => "COMPLETED".into(),
        "FAILED" | "ERROR" => "ERROR".into(),
        "STOPPED" | "STOPPING" | "CANCELLED" | "CANCELED" => "CANCELED".into(),
        other => other.to_string(),
    }
}

/// Submit the job. The submission id is client-chosen (`spec.submission_id`),
/// so a success needs nothing from the response body.
pub async fn run_job(address: &str, spec: &JobSubmission) -> Result<()> {
    let env = super::default_unbuffered(&spec.env);
    let mut runtime_env = json!({ "env_vars": env });
    if let Some(working_dir) = &spec.working_dir {
        runtime_env["working_dir"] = json!(working_dir);
    }
    let mut body = json!({
        "entrypoint": spec.entrypoint,
        "submission_id": spec.submission_id,
        "runtime_env": runtime_env,
        "metadata": spec.metadata,
        "entrypoint_num_cpus": spec.resources.cpus,
        "entrypoint_num_gpus": spec.resources.gpus,
    });
    if let Some(mem) = spec.resources.memory_bytes {
        body["entrypoint_memory"] = json!(mem);
    }
    let res = http()
        .post(format!("{address}/api/jobs/"))
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow!("Could not reach Ray Jobs at {address}: {e}"))?;
    check(res, "job submit").await?;
    Ok(())
}

/// Upload a Ray `working_dir` package through the Jobs server. Ray's own SDK
/// uses this exact content-addressed package endpoint before submitting the
/// job; using the snapshot digest makes the existence check restart-safe.
pub async fn stage_working_dir(
    address: &str,
    digest: &str,
    zip: &std::path::Path,
) -> Result<String> {
    let name = format!("_ray_pkg_{digest}.zip");
    let url = format!("{address}/api/packages/gcs/{name}");
    let existing = http()
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow!("Could not query Ray source package at {address}: {e}"))?;
    if existing.status() != reqwest::StatusCode::OK {
        if existing.status() != reqwest::StatusCode::NOT_FOUND {
            check(existing, "source package lookup").await?;
        }
        let file = tokio::fs::File::open(zip).await?;
        let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file));
        let uploaded = http()
            .put(&url)
            .body(body)
            .send()
            .await
            .map_err(|e| anyhow!("Could not upload Ray source package at {address}: {e}"))?;
        check(uploaded, "source package upload").await?;
    }
    Ok(format!("gcs://{name}"))
}

pub async fn inspect_job(address: &str, submission_id: &str) -> Result<JobInfo> {
    let res = http()
        .get(format!("{address}/api/jobs/{submission_id}"))
        .send()
        .await
        .map_err(|e| anyhow!("Could not reach Ray Jobs at {address}: {e}"))?;
    // A 404 means the cluster no longer knows the job (record purged, head
    // restarted) — a distinct GONE stage for the supervisor to debounce, not
    // a transport error to retry forever.
    if res.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(JobInfo {
            stage: "GONE".to_string(),
            message: None,
        });
    }
    let raw: RawJobStatus = check(res, "job inspect").await?.json().await?;
    let status = raw.status.unwrap_or_else(|| "PENDING".into());
    Ok(JobInfo {
        stage: map_ray_status(&status),
        message: raw.message,
    })
}

pub async fn stop_job(address: &str, submission_id: &str) -> Result<()> {
    let res = http()
        .post(format!("{address}/api/jobs/{submission_id}/stop"))
        .send()
        .await
        .map_err(|e| anyhow!("Could not reach Ray Jobs at {address}: {e}"))?;
    check(res, "job stop").await?;
    Ok(())
}

/// Fetch the full driver log text (Ray returns JSON `{"logs":"..."}` or plain text).
pub async fn fetch_logs(address: &str, submission_id: &str) -> Result<String> {
    let res = http()
        .get(format!("{address}/api/jobs/{submission_id}/logs"))
        .send()
        .await
        .map_err(|e| anyhow!("Could not reach Ray Jobs at {address}: {e}"))?;
    let res = check(res, "job logs").await?;
    let ct = res
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ct.contains("json") {
        let body: serde_json::Value = res.json().await?;
        Ok(body
            .get("logs")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    } else {
        Ok(res.text().await.unwrap_or_default())
    }
}

pub fn job_url(address: &str, submission_id: &str) -> String {
    format!("{address}/#/jobs/{submission_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flavor_defaults_and_gpu() {
        let d = parse_flavor(None).unwrap();
        assert_eq!(d.cpus, 0.0);
        assert_eq!(d.gpus, 0.0);
        let g = parse_flavor(Some("gpu:2,cpu:4,mem:8GiB")).unwrap();
        assert_eq!(g.gpus, 2.0);
        assert_eq!(g.cpus, 4.0);
        assert_eq!(g.memory_bytes, Some(8 * 1024 * 1024 * 1024));
    }

    #[test]
    fn address_prefers_explicit() {
        assert_eq!(
            resolve_address(Some(" http://example:8265/ ")),
            "http://example:8265"
        );
    }

    #[test]
    fn flavor_rejects_nan_and_negative() {
        assert!(parse_flavor(Some("gpu:NaN")).is_err());
        assert!(parse_flavor(Some("cpu:-1")).is_err());
        assert!(parse_flavor(Some("gpu:inf")).is_err());
    }
}
