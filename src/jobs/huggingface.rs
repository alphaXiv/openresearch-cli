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
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use reqwest::Client;
use serde::Deserialize;
use serde_json::json;

use crate::error::{anyhow, Result};

const SOURCE_LAUNCHER: &str = r#"
import json, sys
from huggingface_hub import HfApi

spec = json.load(sys.stdin)
api = HfApi(endpoint=spec["endpoint"], token=spec["token"])
volume = api.sync_job_volume(
    spec["sourceDir"],
    "/orx-source",
    namespace=spec["namespace"],
    remote_name="orx-" + spec["digest"],
    read_only=True,
)
job = api.run_job(
    image=spec["image"],
    command=spec["command"],
    env=spec["environment"],
    secrets=spec["secrets"],
    flavor=spec["flavor"],
    timeout=spec["timeoutSeconds"],
    labels=spec["labels"],
    volumes=[volume],
    namespace=spec["namespace"],
)
print(json.dumps({"id": job.id}))
"#;

fn managed_python() -> PathBuf {
    let env = crate::config::config_dir().join("envs").join("huggingface");
    if cfg!(windows) {
        env.join("Scripts").join("python.exe")
    } else {
        env.join("bin").join("python")
    }
}

async fn ensure_client_env() -> Result<PathBuf> {
    static INSTALL_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    let _install = INSTALL_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let python = managed_python();
    if python.exists() {
        let ready = tokio::process::Command::new(&python)
            .args([
                "-c",
                "from huggingface_hub import HfApi; assert hasattr(HfApi, 'sync_job_volume')",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|status| status.success())
            .unwrap_or(false);
        if ready {
            return Ok(python);
        }
    }
    let base = ["python3", "python"]
        .into_iter()
        .find(|candidate| {
            std::process::Command::new(candidate)
                .args(["-c", "import venv"])
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
        })
        .ok_or_else(|| anyhow!("Python 3 is required to stage source for Hugging Face Jobs."))?;
    let env_dir = python
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| anyhow!("Invalid managed Hugging Face environment path."))?;
    if !python.exists() {
        if let Some(parent) = env_dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let status = tokio::process::Command::new(base)
            .args(["-m", "venv"])
            .arg(env_dir)
            .status()
            .await?;
        if !status.success() {
            return Err(anyhow!(
                "Could not create the Hugging Face client environment."
            ));
        }
    }
    eprintln!("orx: installing the Hugging Face source-transfer client (one time)…");
    let status = tokio::process::Command::new(&python)
        .args([
            "-m",
            "pip",
            "install",
            "--quiet",
            "--disable-pip-version-check",
            "huggingface_hub>=1.8.0",
        ])
        .status()
        .await?;
    if !status.success() {
        return Err(anyhow!(
            "Could not install the Hugging Face source-transfer client."
        ));
    }
    Ok(python)
}

pub fn endpoint() -> String {
    std::env::var("HF_ENDPOINT").unwrap_or_else(|_| "https://huggingface.co".to_string())
}

/// Which link of the resolution chain produced the token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    Env,
    OpenresearchEnv,
    HfCache,
}

/// Resolve the HF token: `HF_TOKEN` env first, then the box's synced env file
/// (`~/.openresearch/env` — where the org credential/env-var lands, invisible
/// to non-interactive shells), then the hf CLI's token file.
pub fn resolve_token() -> Result<String> {
    resolve_token_with_source().map(|(tok, _)| tok)
}

/// `resolve_token`, but also reports which source won (settings UI).
pub fn resolve_token_with_source() -> Result<(String, TokenSource)> {
    if let Ok(tok) = std::env::var("HF_TOKEN") {
        let tok = tok.trim().to_string();
        if !tok.is_empty() {
            return Ok((tok, TokenSource::Env));
        }
    }
    if let Some(tok) = crate::config::synced_env_var("HF_TOKEN") {
        return Ok((tok, TokenSource::OpenresearchEnv));
    }
    let path = dirs::home_dir()
        .unwrap_or_default()
        .join(".cache")
        .join("huggingface")
        .join("token");
    if let Ok(tok) = std::fs::read_to_string(&path) {
        let tok = tok.trim().to_string();
        if !tok.is_empty() {
            return Ok((tok, TokenSource::HfCache));
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

/// whoami-v2 details for the settings UI.
#[derive(Debug, Clone)]
pub struct WhoamiDetails {
    pub name: String,
    /// Can this token submit jobs? `None` = shape didn't say.
    pub jobs_write: Option<bool>,
}

pub async fn whoami_details(token: &str) -> Result<WhoamiDetails> {
    let res = http()
        .get(format!("{}/api/whoami-v2", endpoint()))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| anyhow!("Could not reach Hugging Face: {}", e))?;
    let body: serde_json::Value = check(res, "whoami").await?.json().await?;
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("whoami response had no name"))?
        .to_string();
    let jobs_write = match body["auth"]["accessToken"]["role"].as_str() {
        Some("write") => Some(true),
        Some("read") => Some(false),
        // fineGrained: job.write must appear in the global or a scoped grant.
        Some("fineGrained") => {
            let fg = &body["auth"]["accessToken"]["fineGrained"];
            let global = fg.get("global").and_then(|v| v.as_array());
            let scoped = fg.get("scoped").and_then(|v| v.as_array());
            if global.is_none() && scoped.is_none() {
                None
            } else {
                let has = |perms: &[serde_json::Value]| {
                    perms.iter().any(|p| p.as_str() == Some("job.write"))
                };
                let hit = global.map(|g| has(g)).unwrap_or(false)
                    || scoped.into_iter().flatten().any(|s| {
                        s.get("permissions")
                            .and_then(|p| p.as_array())
                            .map(|p| has(p))
                            .unwrap_or(false)
                    });
                Some(hit)
            }
        }
        _ => None,
    };
    Ok(WhoamiDetails { name, jobs_write })
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

/// Sync the immutable archive into the private `jobs-artifacts` bucket and
/// mount it into the Job. The digest-derived remote name makes retries upload
/// nothing when the exact source was already staged.
pub async fn run_job_with_source(
    token: &str,
    namespace: &str,
    spec: &JobSubmission,
    archive: &Path,
    digest: &str,
) -> Result<JobInfo> {
    let python = ensure_client_env().await?;
    let source_dir = archive
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{digest}.hf"));
    std::fs::create_dir_all(&source_dir)?;
    let staged = source_dir.join("source.tar");
    if !staged.exists() && std::fs::hard_link(archive, &staged).is_err() {
        std::fs::copy(archive, &staged)?;
    }
    let body = json!({
        "endpoint": endpoint(),
        "token": token,
        "namespace": namespace,
        "sourceDir": source_dir,
        "digest": digest,
        "image": spec.docker_image,
        "command": spec.command,
        "environment": super::default_unbuffered(&spec.environment),
        "secrets": spec.secrets,
        "flavor": spec.flavor,
        "timeoutSeconds": spec.timeout_seconds,
        "labels": spec.labels,
    });
    let mut child = tokio::process::Command::new(python)
        .args(["-c", SOURCE_LAUNCHER])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    use tokio::io::AsyncWriteExt as _;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(body.to_string().as_bytes())
        .await?;
    let output = child.wait_with_output().await?;
    if !output.status.success() {
        return Err(anyhow!(
            "Hugging Face source staging failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let id = value["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow!("Hugging Face source launch returned no job id."))?;
    Ok(JobInfo {
        id: id.to_string(),
        status: JobStatus {
            stage: "SCHEDULING".to_string(),
            message: None,
        },
    })
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
