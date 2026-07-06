//! Kubernetes Jobs backend — the user's own cluster via kubectl shell-outs.
//!
//! Everything goes through the `kubectl` binary rather than a client crate:
//! it inherits the user's kubeconfig auth verbatim (including exec plugins,
//! which managed clusters like CoreWeave/EKS/GKE rely on) at zero dependency
//! cost. A run is a batch/v1 Job: `backoffLimit: 0` (a failed run fails, no
//! silent retries), `activeDeadlineSeconds` for the timeout, and a 24h
//! `ttlSecondsAfterFinished` so finished Jobs clean themselves up after the
//! supervisor has drained logs.
//!
//! Job stages are mapped onto the HF stage vocabulary (SCHEDULING/RUNNING/
//! COMPLETED/ERROR/CANCELED/DELETED) so `stage_to_run_status` and
//! `is_terminal_stage` in `jobs/mod.rs` apply unchanged.

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;

use crate::error::{anyhow, Result};

/// Env vars land in this namespace-local Secret; jobs mount it via `envFrom`
/// (`optional: true`, so an empty env file is fine). Re-synced on every
/// launch; pods read it once at start.
const ENV_SECRET: &str = "orx-env";

// --- settings ---------------------------------------------------------------

/// User-tunable k8s settings, stored at
/// `$XDG_CONFIG_HOME/openresearch/k8s.json`. No secrets in here — kubectl
/// holds all auth.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct K8sSettings {
    /// kubeconfig context; `None` = kubectl's current-context.
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default = "default_namespace")]
    pub namespace: String,
    /// Default docker image; `None` = per-flavor default (CUDA pytorch on
    /// GPU flavors, plain python otherwise).
    #[serde(default)]
    pub default_image: Option<String>,
    /// Auto-detected from node shapes; refreshed via settings "detect".
    #[serde(default)]
    pub flavors: Vec<Flavor>,
    /// User-defined flavors; win over detected ones on name clash.
    #[serde(default)]
    pub custom_flavors: Vec<Flavor>,
    #[serde(default)]
    pub detected_at: Option<i64>,
}

fn default_namespace() -> String {
    "default".to_string()
}

impl Default for K8sSettings {
    fn default() -> Self {
        Self {
            context: None,
            namespace: default_namespace(),
            default_image: None,
            flavors: Vec::new(),
            custom_flavors: Vec::new(),
            detected_at: None,
        }
    }
}

impl K8sSettings {
    /// Custom flavors first (they win on clash), then detected.
    pub fn all_flavors(&self) -> Vec<Flavor> {
        let mut out = self.custom_flavors.clone();
        for f in &self.flavors {
            if !out.iter().any(|c| c.name == f.name) {
                out.push(f.clone());
            }
        }
        out
    }

    pub fn resolve_flavor(&self, name: &str) -> Option<Flavor> {
        self.all_flavors().into_iter().find(|f| f.name == name)
    }
}

/// A launchable resource shape: the k8s analog of an HF flavor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Flavor {
    pub name: String,
    pub gpu: u64,
    /// k8s CPU quantity, e.g. "14" or "500m".
    pub cpu: String,
    /// k8s memory quantity, e.g. "118Gi".
    pub memory: String,
}

fn settings_path() -> std::path::PathBuf {
    crate::config::config_dir().join("k8s.json")
}

/// `Ok(None)` when the file is missing or unreadable — k8s not configured.
pub fn load_settings() -> Result<Option<K8sSettings>> {
    let raw = match std::fs::read_to_string(settings_path()) {
        Ok(raw) => raw,
        Err(_) => return Ok(None),
    };
    match serde_json::from_str::<K8sSettings>(&raw) {
        Ok(s) => Ok(Some(s)),
        Err(e) => Err(anyhow!(
            "Unreadable {} ({}). Fix or delete it and reconfigure.",
            settings_path().display(),
            e
        )),
    }
}

pub fn save_settings(settings: &K8sSettings) -> Result<()> {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = format!("{}\n", serde_json::to_string_pretty(settings)?);
    std::fs::write(&path, body)?;
    Ok(())
}

// --- kubectl plumbing ---------------------------------------------------------

/// Run kubectl with the given args (plus `--context` when set), feeding
/// `stdin` if provided. Non-zero exit → Err carrying stderr.
async fn kubectl(context: Option<&str>, args: &[&str], stdin: Option<&str>) -> Result<String> {
    let mut cmd = Command::new("kubectl");
    if let Some(ctx) = context {
        cmd.arg("--context").arg(ctx);
    }
    cmd.args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow!("kubectl not found on PATH — install it to use --backend k8s.")
        } else {
            anyhow!("Could not run kubectl: {}", e)
        }
    })?;
    if let Some(body) = stdin {
        let mut pipe = child.stdin.take().expect("piped stdin");
        pipe.write_all(body.as_bytes()).await?;
        drop(pipe); // close so kubectl sees EOF
    }
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!("kubectl {} failed: {}", args.join(" "), err.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// All kubeconfig contexts plus the current one (None when unset).
pub async fn list_contexts() -> Result<(Vec<String>, Option<String>)> {
    let all = kubectl(None, &["config", "get-contexts", "-o", "name"], None).await?;
    let contexts: Vec<String> = all
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    let current = kubectl(None, &["config", "current-context"], None)
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Ok((contexts, current))
}

/// Cluster-facing health for the settings UI and the startup warning.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preflight {
    pub kubectl_found: bool,
    pub reachable: bool,
    pub can_create_jobs: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub async fn preflight(context: Option<&str>, namespace: &str) -> Preflight {
    if kubectl(None, &["version", "--client"], None).await.is_err() {
        return Preflight {
            kubectl_found: false,
            reachable: false,
            can_create_jobs: false,
            error: Some("kubectl not found on PATH".to_string()),
        };
    }
    // `auth can-i` answers reachability and permission in one round trip:
    // "yes"/"no" on stdout both mean the API server responded (it exits 1 on
    // "no", so run it raw rather than through the exit-code-checking runner).
    let mut cmd = Command::new("kubectl");
    if let Some(ctx) = context {
        cmd.arg("--context").arg(ctx);
    }
    let out = cmd
        .args(["auth", "can-i", "create", "jobs", "-n", namespace])
        .stdin(Stdio::null())
        .output()
        .await;
    match out {
        Ok(out) => {
            let answer = String::from_utf8_lossy(&out.stdout);
            match answer.trim() {
                "yes" => Preflight {
                    kubectl_found: true,
                    reachable: true,
                    can_create_jobs: true,
                    error: None,
                },
                "no" => Preflight {
                    kubectl_found: true,
                    reachable: true,
                    can_create_jobs: false,
                    error: None,
                },
                _ => Preflight {
                    kubectl_found: true,
                    reachable: false,
                    can_create_jobs: false,
                    error: Some(String::from_utf8_lossy(&out.stderr).trim().to_string()),
                },
            }
        }
        Err(e) => Preflight {
            kubectl_found: true,
            reachable: false,
            can_create_jobs: false,
            error: Some(e.to_string()),
        },
    }
}

// --- flavor detection ---------------------------------------------------------

/// Millicores from a k8s CPU quantity ("128", "127960m").
fn parse_cpu_millis(q: &str) -> u64 {
    let q = q.trim();
    if let Some(m) = q.strip_suffix('m') {
        m.parse().unwrap_or(0)
    } else {
        q.parse::<f64>().map(|c| (c * 1000.0) as u64).unwrap_or(0)
    }
}

/// KiB from a k8s memory quantity ("1055333312Ki", "1Ti", "16Gi", bytes).
fn parse_mem_kib(q: &str) -> u64 {
    let q = q.trim();
    let (num, mul): (&str, u64) = if let Some(n) = q.strip_suffix("Ki") {
        (n, 1)
    } else if let Some(n) = q.strip_suffix("Mi") {
        (n, 1024)
    } else if let Some(n) = q.strip_suffix("Gi") {
        (n, 1024 * 1024)
    } else if let Some(n) = q.strip_suffix("Ti") {
        (n, 1024 * 1024 * 1024)
    } else if let Some(n) = q.strip_suffix('K') {
        (n, 1)
    } else if let Some(n) = q.strip_suffix('M') {
        (n, 1000)
    } else if let Some(n) = q.strip_suffix('G') {
        (n, 1_000_000)
    } else {
        return q.parse::<u64>().map(|b| b / 1024).unwrap_or(0);
    };
    num.parse::<f64>()
        .map(|n| (n * mul as f64) as u64)
        .unwrap_or(0)
}

fn fmt_cpu(millis: u64) -> String {
    if millis >= 1000 {
        format!("{}", millis / 1000)
    } else {
        format!("{}m", millis.max(100))
    }
}

fn fmt_mem(kib: u64) -> String {
    let gib = kib / (1024 * 1024);
    if gib >= 1 {
        format!("{gib}Gi")
    } else {
        format!("{}Mi", (kib / 1024).max(256))
    }
}

/// Short flavor prefix from the GPU product label: lowercase alnum segments
/// up to and including the first one with a digit — "NVIDIA-H100-80GB-HBM3"
/// → "h100", "NVIDIA-RTX-PRO-6000-Blackwell-…" → "rtxpro6000".
fn short_gpu_name(product: &str) -> String {
    let mut out = String::new();
    for seg in product.split(['-', '_', ' ']) {
        let seg: String = seg
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .flat_map(char::to_lowercase)
            .collect();
        if seg.is_empty() || seg == "nvidia" {
            continue;
        }
        let has_digit = seg.chars().any(|c| c.is_ascii_digit());
        out.push_str(&seg);
        if has_digit {
            break;
        }
    }
    if out.is_empty() {
        "gpu".to_string()
    } else {
        out
    }
}

/// Derive flavors from live node shapes: for each (GPU product, GPU count)
/// family, power-of-two GPU slices with a proportional share of the family's
/// smallest node's CPU/memory × 0.9 headroom (daemonsets nibble at
/// allocatable, and a zero-headroom full-node pod would never schedule).
pub async fn detect_flavors(context: Option<&str>) -> Result<Vec<Flavor>> {
    let raw = kubectl(context, &["get", "nodes", "-o", "json"], None).await?;
    let nodes: Value = serde_json::from_str(&raw)?;

    // family key → (gpu, min cpu millis, min mem kib)
    let mut families: HashMap<String, (u64, u64, u64)> = HashMap::new();
    for node in nodes["items"].as_array().unwrap_or(&Vec::new()) {
        let alloc = &node["status"]["allocatable"];
        let gpu: u64 = alloc["nvidia.com/gpu"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if gpu == 0 {
            continue;
        }
        let cpu_m = parse_cpu_millis(alloc["cpu"].as_str().unwrap_or("0"));
        let mem_ki = parse_mem_kib(alloc["memory"].as_str().unwrap_or("0"));
        let product = node["metadata"]["labels"]["nvidia.com/gpu.product"]
            .as_str()
            .unwrap_or("");
        let key = format!("{}/{}", short_gpu_name(product), gpu);
        let entry = families.entry(key).or_insert((gpu, u64::MAX, u64::MAX));
        entry.1 = entry.1.min(cpu_m);
        entry.2 = entry.2.min(mem_ki);
    }

    let mut flavors = Vec::new();
    let mut keys: Vec<&String> = families.keys().collect();
    keys.sort();
    for key in keys {
        let (gpus, cpu_m, mem_ki) = families[key];
        let base = key.split('/').next().unwrap_or("gpu");
        let mut n = 1u64;
        let mut slices = Vec::new();
        while n < gpus {
            slices.push(n);
            n *= 2;
        }
        slices.push(gpus);
        for n in slices {
            flavors.push(Flavor {
                name: format!("{base}x{n}"),
                gpu: n,
                cpu: fmt_cpu(cpu_m * n * 9 / (gpus * 10)),
                memory: fmt_mem(mem_ki * n * 9 / (gpus * 10)),
            });
        }
    }
    // A small CPU-only flavor is always offered; overridable in settings.
    flavors.push(Flavor {
        name: "cpu-small".to_string(),
        gpu: 0,
        cpu: "4".to_string(),
        memory: "16Gi".to_string(),
    });
    Ok(flavors)
}

// --- job lifecycle ------------------------------------------------------------

pub struct K8sJobSpec {
    /// `bash -c` payload (the shared clone-and-run script).
    pub script: String,
    pub image: String,
    pub flavor: Flavor,
    /// Synced into the `orx-env` Secret and exposed to the pod via envFrom.
    pub env: HashMap<String, String>,
    pub timeout_seconds: u64,
    pub labels: HashMap<String, String>,
}

/// Sync the env Secret, then create the Job. Returns the generated job name.
pub async fn run_job(context: Option<&str>, namespace: &str, spec: &K8sJobSpec) -> Result<String> {
    if !spec.env.is_empty() {
        // stringData via stdin — values never appear on a command line.
        let secret = json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": { "name": ENV_SECRET, "namespace": namespace,
                          "labels": { "app.kubernetes.io/managed-by": "orx" } },
            "type": "Opaque",
            "stringData": spec.env,
        });
        kubectl(context, &["apply", "-f", "-"], Some(&secret.to_string())).await?;
    }

    // cpu/memory are requests-only (limits would throttle and OOM-kill);
    // the GPU count must appear in limits — that's what the device plugin reads.
    let mut requests = json!({ "cpu": spec.flavor.cpu, "memory": spec.flavor.memory });
    let mut limits = json!({});
    if spec.flavor.gpu > 0 {
        requests["nvidia.com/gpu"] = json!(spec.flavor.gpu.to_string());
        limits["nvidia.com/gpu"] = json!(spec.flavor.gpu.to_string());
    }
    let manifest = json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {
            "generateName": "orx-",
            "namespace": namespace,
            "labels": spec.labels,
        },
        "spec": {
            "backoffLimit": 0,
            "ttlSecondsAfterFinished": 86400,
            "activeDeadlineSeconds": spec.timeout_seconds,
            "template": {
                "metadata": { "labels": spec.labels },
                "spec": {
                    "restartPolicy": "Never",
                    "containers": [{
                        "name": "run",
                        "image": spec.image,
                        "command": ["bash", "-c", spec.script],
                        "resources": { "requests": requests, "limits": limits },
                        "envFrom": [{ "secretRef": { "name": ENV_SECRET, "optional": true } }],
                    }],
                },
            },
        },
    });
    let name = kubectl(
        context,
        &["create", "-f", "-", "-o", "jsonpath={.metadata.name}"],
        Some(&manifest.to_string()),
    )
    .await?;
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err(anyhow!("kubectl create returned no job name"));
    }
    Ok(name)
}

/// Job state in the shared stage vocabulary.
#[derive(Debug, Clone)]
pub struct JobState {
    pub stage: String,
    pub message: Option<String>,
}

pub async fn inspect_job(context: Option<&str>, namespace: &str, name: &str) -> Result<JobState> {
    let raw = match kubectl(
        context,
        &["get", "job", name, "-n", namespace, "-o", "json"],
        None,
    )
    .await
    {
        Ok(raw) => raw,
        // A deleted Job is how cancel manifests (and how TTL cleanup looks
        // long after terminal — supervise has exited by then).
        Err(e) if e.to_string().contains("NotFound") => {
            return Ok(JobState {
                stage: "DELETED".to_string(),
                message: None,
            })
        }
        Err(e) => return Err(e),
    };
    let job: Value = serde_json::from_str(&raw)?;
    let status = &job["status"];

    let empty = Vec::new();
    let conditions = status["conditions"].as_array().unwrap_or(&empty);
    let condition = |ty: &str| -> Option<&Value> {
        conditions
            .iter()
            .find(|c| c["type"] == ty && c["status"] == "True")
    };
    if condition("Complete").is_some() || status["succeeded"].as_u64().unwrap_or(0) > 0 {
        return Ok(JobState {
            stage: "COMPLETED".to_string(),
            message: None,
        });
    }
    if let Some(failed) = condition("Failed") {
        let reason = failed["reason"].as_str().unwrap_or("");
        let msg = failed["message"].as_str().unwrap_or("");
        return Ok(JobState {
            stage: "ERROR".to_string(),
            message: Some(
                format!("{reason}: {msg}")
                    .trim_matches([':', ' '])
                    .to_string(),
            ),
        });
    }

    // Live: RUNNING once a pod runs, SCHEDULING before that (surfacing pod
    // waiting reasons like ImagePullBackOff so "stuck" is diagnosable).
    let pods_raw = kubectl(
        context,
        &[
            "get",
            "pods",
            "-n",
            namespace,
            "-l",
            &format!("job-name={name}"),
            "-o",
            "json",
        ],
        None,
    )
    .await
    .unwrap_or_default();
    let pods: Value = serde_json::from_str(&pods_raw).unwrap_or_else(|_| json!({"items": []}));
    let mut waiting_msg = None;
    for pod in pods["items"].as_array().unwrap_or(&empty) {
        match pod["status"]["phase"].as_str() {
            Some("Running") | Some("Succeeded") => {
                return Ok(JobState {
                    stage: "RUNNING".to_string(),
                    message: None,
                })
            }
            _ => {}
        }
        for cs in pod["status"]["containerStatuses"]
            .as_array()
            .unwrap_or(&empty)
        {
            if let Some(reason) = cs["state"]["waiting"]["reason"].as_str() {
                waiting_msg = Some(reason.to_string());
            }
        }
    }
    Ok(JobState {
        stage: "SCHEDULING".to_string(),
        message: waiting_msg,
    })
}

/// Cancel = delete the Job (cascades to its pod). Missing job → already gone.
pub async fn cancel_job(context: Option<&str>, namespace: &str, name: &str) -> Result<()> {
    match kubectl(
        context,
        &["delete", "job", name, "-n", namespace, "--wait=false"],
        None,
    )
    .await
    {
        Ok(_) => Ok(()),
        Err(e) if e.to_string().contains("NotFound") => Ok(()),
        Err(e) => Err(e),
    }
}

/// One pass over `kubectl logs -f`, invoking `sink` per line past `skip`.
///
/// Same replay/dedup contract as `hf::stream_logs`: kubectl replays the log
/// from the start on each (re)connect, so the caller passes how many lines it
/// has consumed and gets the new total back. Ends when kubectl exits (pod
/// gone/finished, or not yet started — it errors fast) or after `idle`
/// silence; the supervisor re-checks job state and reconnects if still live.
pub async fn stream_logs(
    context: Option<&str>,
    namespace: &str,
    job_name: &str,
    skip: u64,
    idle: Duration,
    sink: &mut (dyn FnMut(&str) + Send),
) -> Result<u64> {
    use tokio::io::{AsyncBufReadExt as _, BufReader};

    let mut cmd = Command::new("kubectl");
    if let Some(ctx) = context {
        cmd.arg("--context").arg(ctx);
    }
    let mut child = cmd
        .args([
            "logs",
            "-f",
            &format!("job/{job_name}"),
            "-n",
            namespace,
            "--tail=-1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null()) // "waiting to start" noise; state comes from inspect
        .spawn()
        .map_err(|e| anyhow!("Could not run kubectl logs: {}", e))?;

    let stdout = child.stdout.take().expect("piped stdout");
    let mut lines = BufReader::new(stdout).lines();
    let mut seen = 0u64;
    loop {
        match tokio::time::timeout(idle, lines.next_line()).await {
            Err(_) => break,       // idle — let the caller re-check state
            Ok(Err(_)) => break,   // read error
            Ok(Ok(None)) => break, // kubectl exited
            Ok(Ok(Some(line))) => {
                seen += 1;
                if seen > skip {
                    sink(&line);
                }
            }
        }
    }
    let _ = child.kill().await;
    Ok(seen.max(skip))
}
