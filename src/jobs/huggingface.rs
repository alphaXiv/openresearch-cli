//! Hugging Face Jobs client — the REST surface behind `hf jobs`.
//!
//! Paths and body shapes mirror huggingface_hub's `hf_api.py`/`_jobs_api.py`:
//!   POST {endpoint}/api/jobs/{namespace}            run a job
//!   GET  {endpoint}/api/jobs/{namespace}/{id}       inspect
//!   GET  {endpoint}/api/jobs/{namespace}/{id}/logs  SSE log stream
//!   POST {endpoint}/api/jobs/{namespace}/{id}/cancel
//!   GET  {endpoint}/api/whoami-v2                   token → namespace
//! Wire fields are camelCase; `timeoutSeconds` is integer seconds; auth is a
//! plain `Bearer` header on every call including the log stream.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use reqwest::Client;
use serde::Deserialize;
use serde_json::json;

use crate::error::{anyhow, Result};

pub fn endpoint() -> String {
    std::env::var("HF_ENDPOINT").unwrap_or_else(|_| "https://huggingface.co".to_string())
}

/// Resolve the HF token: `HF_TOKEN` env first, then the box's synced env file
/// (`~/.openresearch/env` — where the org credential/env-var lands, invisible
/// to non-interactive shells), then the hf CLI's token file.
pub fn resolve_token() -> Result<String> {
    if let Ok(tok) = std::env::var("HF_TOKEN") {
        let tok = tok.trim().to_string();
        if !tok.is_empty() {
            return Ok(tok);
        }
    }
    if let Some(tok) = crate::config::synced_env_var("HF_TOKEN") {
        return Ok(tok);
    }
    let path = dirs::home_dir()
        .unwrap_or_default()
        .join(".cache")
        .join("huggingface")
        .join("token");
    if let Ok(tok) = std::fs::read_to_string(&path) {
        let tok = tok.trim().to_string();
        if !tok.is_empty() {
            return Ok(tok);
        }
    }
    Err(anyhow!(
        "No Hugging Face token found. Set HF_TOKEN (or run `hf auth login`). \
         Mint one at https://huggingface.co/settings/tokens — or connect it in \
         the org's compute settings so it syncs to agent boxes automatically."
    ))
}

fn http() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .build()
            .expect("reqwest client")
    })
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobStatus {
    pub stage: String,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobInfo {
    pub id: String,
    pub status: JobStatus,
}

async fn check(res: reqwest::Response, what: &str) -> Result<reqwest::Response> {
    let status = res.status();
    if status.is_success() {
        return Ok(res);
    }
    let body = res.text().await.unwrap_or_default();
    if status.as_u16() == 401 {
        return Err(anyhow!(
            "Hugging Face rejected the token (HTTP 401) during {what}. Check HF_TOKEN."
        ));
    }
    Err(anyhow!(
        "Hugging Face {} failed ({}): {}",
        what,
        status.as_u16(),
        body
    ))
}

/// The token's account name — the default jobs namespace. whoami-v2 is heavily
/// rate-limited upstream, so call once per command, not per poll.
pub async fn whoami(token: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct WhoAmI {
        name: String,
    }
    let res = http()
        .get(format!("{}/api/whoami-v2", endpoint()))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| anyhow!("Could not reach Hugging Face: {}", e))?;
    let who: WhoAmI = check(res, "whoami").await?.json().await?;
    Ok(who.name)
}

pub struct JobSubmission {
    pub command: Vec<String>,
    pub docker_image: String,
    pub flavor: String,
    pub environment: HashMap<String, String>,
    pub secrets: HashMap<String, String>,
    pub timeout_seconds: u64,
    pub labels: HashMap<String, String>,
}

pub async fn run_job(token: &str, namespace: &str, spec: &JobSubmission) -> Result<JobInfo> {
    // Mirror the python client: arguments/environment always present.
    let mut body = json!({
        "command": spec.command,
        "arguments": [],
        "environment": spec.environment,
        "flavor": spec.flavor,
        "dockerImage": spec.docker_image,
        "timeoutSeconds": spec.timeout_seconds,
    });
    if !spec.secrets.is_empty() {
        body["secrets"] = json!(spec.secrets);
    }
    if !spec.labels.is_empty() {
        body["labels"] = json!(spec.labels);
    }
    let res = http()
        .post(format!("{}/api/jobs/{}", endpoint(), namespace))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow!("Could not reach Hugging Face: {}", e))?;
    let job: JobInfo = check(res, "job submit").await?.json().await?;
    Ok(job)
}

pub async fn inspect_job(token: &str, namespace: &str, job_id: &str) -> Result<JobInfo> {
    let res = http()
        .get(format!("{}/api/jobs/{}/{}", endpoint(), namespace, job_id))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| anyhow!("Could not reach Hugging Face: {}", e))?;
    let job: JobInfo = check(res, "job inspect").await?.json().await?;
    Ok(job)
}

pub async fn cancel_job(token: &str, namespace: &str, job_id: &str) -> Result<()> {
    let res = http()
        .post(format!(
            "{}/api/jobs/{}/{}/cancel",
            endpoint(),
            namespace,
            job_id
        ))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| anyhow!("Could not reach Hugging Face: {}", e))?;
    check(res, "job cancel").await?;
    Ok(())
}

/// One pass over the job's SSE log stream, invoking `sink` per log line.
///
/// `skip` dedups replayed history on reconnect: the server replays the stream
/// from the start each time, so the caller passes how many data events it has
/// already consumed. Returns the new total. Ends when the server closes the
/// stream or nothing arrives for `idle_timeout` (the supervisor then re-checks
/// job state and reconnects if it's still live).
pub async fn stream_logs(
    token: &str,
    namespace: &str,
    job_id: &str,
    skip: u64,
    idle_timeout: Duration,
    sink: &mut (dyn FnMut(&str) + Send),
) -> Result<u64> {
    #[derive(Deserialize)]
    struct LogEvent {
        data: String,
    }
    let res = http()
        .get(format!(
            "{}/api/jobs/{}/{}/logs",
            endpoint(),
            namespace,
            job_id
        ))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| anyhow!("Could not reach Hugging Face: {}", e))?;
    let mut res = check(res, "log stream").await?;

    let mut seen = 0u64;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let chunk = match tokio::time::timeout(idle_timeout, res.chunk()).await {
            Err(_) => break,       // idle — likely end of buffered history
            Ok(Err(_)) => break,   // stream error — caller reconnects if live
            Ok(Ok(None)) => break, // server closed
            Ok(Ok(Some(c))) => c,
        };
        buf.extend_from_slice(&chunk);
        // SSE frames are newline-delimited; process complete lines only.
        while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end();
            let Some(json_part) = line.strip_prefix("data: {") else {
                continue; // keep-alive comments, event: lines, blanks
            };
            let Ok(event) = serde_json::from_str::<LogEvent>(&format!("{{{json_part}")) else {
                continue;
            };
            if event.data.starts_with("===== Job started") {
                continue;
            }
            seen += 1;
            if seen <= skip {
                continue;
            }
            sink(&event.data);
        }
    }
    Ok(seen.max(skip))
}

/// Parse a human duration ("90s", "30m", "4h", "1d", or bare seconds).
pub fn parse_timeout(value: &str) -> Result<u64> {
    let v = value.trim();
    let (num, factor) = match v.chars().last() {
        Some('s') => (&v[..v.len() - 1], 1u64),
        Some('m') => (&v[..v.len() - 1], 60),
        Some('h') => (&v[..v.len() - 1], 3600),
        Some('d') => (&v[..v.len() - 1], 86_400),
        _ => (v, 1),
    };
    let n: u64 = num
        .parse()
        .map_err(|_| anyhow!("Bad --timeout '{}': use e.g. 30m, 4h, 1d.", value))?;
    Ok(n * factor)
}

/// Where to watch the job on huggingface.co.
pub fn job_url(namespace: &str, job_id: &str) -> String {
    format!("{}/jobs/{}/{}", endpoint(), namespace, job_id)
}
