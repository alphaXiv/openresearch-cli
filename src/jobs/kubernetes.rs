//! Kubernetes backend — the user's own cluster via kubectl shell-outs.
//!
//! Everything goes through the `kubectl` binary rather than a client crate:
//! it inherits the user's kubeconfig auth verbatim (including exec plugins,
//! which managed clusters like CoreWeave/EKS/GKE rely on) at zero dependency
//! cost.
//!
//! There is deliberately no resource abstraction (no flavors, no topology
//! knobs): a run is a **manifest committed on the experiment branch**, so
//! compute shape is versioned, diffable code like everything else in the
//! tree. orx owns only the run contract, not the shape:
//!
//! - the manifest must contain exactly one Job (or mark one of several with
//!   the `orx-primary: "true"` label) — its completion/failure is the run's;
//! - the Job's container command must reference `$ORX_SCRIPT`, the injected
//!   env var holding the clone-and-run script (the run command stays the
//!   experiment's fixed contract);
//! - orx injects run labels, the `orx-env` Secret ref, and defaults for
//!   `activeDeadlineSeconds` / `ttlSecondsAfterFinished` / `backoffLimit`
//!   when the manifest doesn't set them;
//! - `{{ORX_RUN}}` in the manifest text is replaced with a run-unique,
//!   DNS-safe id — use it in resource names so re-runs don't collide;
//! - every applied resource is recorded on the run, and cancel deletes
//!   exactly that list.
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

/// Env vars land in this namespace-local Secret; the primary Job gets an
/// `envFrom` ref injected (`optional: true`, so an empty env file is fine);
/// auxiliary resources reference it themselves if they need the keys.
/// Re-synced on every launch; pods read it once at start.
pub const ENV_SECRET: &str = "orx-env";

/// Label that picks the primary Job when a manifest contains several.
const PRIMARY_LABEL: &str = "orx-primary";

// --- settings ---------------------------------------------------------------

/// User-tunable k8s settings, stored at
/// `$XDG_CONFIG_HOME/openresearch/k8s.json`. No secrets in here — kubectl
/// holds all auth. (Older files may carry extra fields like `flavors`; they
/// parse fine and are dropped on the next save.)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct K8sSettings {
    /// kubeconfig context; `None` = kubectl's current-context.
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default = "default_namespace")]
    pub namespace: String,
}

fn default_namespace() -> String {
    "default".to_string()
}

impl Default for K8sSettings {
    fn default() -> Self {
        Self {
            context: None,
            namespace: default_namespace(),
        }
    }
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

// --- manifest submission --------------------------------------------------------

pub struct ManifestSpec {
    /// Raw manifest text as committed on the experiment branch (YAML or JSON,
    /// multi-document fine).
    pub manifest: String,
    /// Clone-and-run script; injected as the `ORX_SCRIPT` env var on the
    /// primary Job's containers.
    pub script: String,
    /// Run-unique DNS-safe token substituted for `{{ORX_RUN}}`.
    pub run_token: String,
    /// Synced into the `orx-env` Secret.
    pub env: HashMap<String, String>,
    /// Injected as `activeDeadlineSeconds` when the manifest doesn't set one.
    pub timeout_seconds: u64,
    pub labels: HashMap<String, String>,
}

pub struct Submitted {
    /// The primary Job's name (post-`{{ORX_RUN}}` substitution).
    pub job_name: String,
    /// Everything created, as `kind/name`, in creation order.
    pub resources: Vec<String>,
}

/// Sync the env Secret, then validate and create the manifest's resources.
/// On a partial failure everything already created is rolled back.
pub async fn run_manifest(
    context: Option<&str>,
    namespace: &str,
    spec: &ManifestSpec,
) -> Result<Submitted> {
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

    let rendered = spec.manifest.replace("{{ORX_RUN}}", &spec.run_token);
    // YAML → JSON plus schema sanity in one step, without touching the
    // cluster and without a YAML dependency.
    let converted = kubectl(
        context,
        &[
            "create",
            "--dry-run=client",
            "-n",
            namespace,
            "-f",
            "-",
            "-o",
            "json",
        ],
        Some(&rendered),
    )
    .await
    .map_err(|e| anyhow!("manifest failed client-side validation: {e}"))?;
    // Multi-doc manifests come back as *concatenated* JSON objects, not a
    // List — read the stream.
    let mut converted_docs: Vec<Value> = Vec::new();
    for v in serde_json::Deserializer::from_str(&converted).into_iter::<Value>() {
        converted_docs.push(v?);
    }

    let (docs, job_name) = prepare_docs(
        converted_docs,
        namespace,
        &spec.script,
        spec.timeout_seconds,
        &spec.labels,
    )?;

    // Create one resource at a time so a failure can roll back exactly what
    // exists so far (a multi-doc `kubectl create` stops mid-way but doesn't
    // undo).
    let mut created: Vec<String> = Vec::new();
    for doc in &docs {
        let handle = resource_handle(doc);
        if let Err(e) = kubectl(context, &["create", "-f", "-"], Some(&doc.to_string())).await {
            for r in created.iter().rev() {
                let _ = delete_resources(context, namespace, std::slice::from_ref(r)).await;
            }
            return Err(anyhow!("could not create {handle}: {e}"));
        }
        created.push(handle);
    }
    Ok(Submitted {
        job_name,
        resources: created,
    })
}

fn resource_handle(doc: &Value) -> String {
    format!(
        "{}/{}",
        doc["kind"].as_str().unwrap_or("").to_lowercase(),
        doc["metadata"]["name"].as_str().unwrap_or("")
    )
}

/// Lint the converted manifest and inject the orx-owned pieces. Returns the
/// docs ready to create plus the primary Job's name.
///
/// Lint rules (submit-time, loud): at least one doc; every doc named (no
/// generateName — orx must know what it created); no foreign namespace;
/// exactly one Job, or exactly one labelled `orx-primary: "true"`; the
/// primary Job's containers must reference `$ORX_SCRIPT`.
///
/// Injections: run labels on every doc (and the primary Job's pod template);
/// `ORX_SCRIPT` env + `orx-env` envFrom on the primary Job's containers;
/// `activeDeadlineSeconds` / `ttlSecondsAfterFinished` / `backoffLimit`
/// defaults on the primary Job when absent.
fn prepare_docs(
    converted: Vec<Value>,
    namespace: &str,
    script: &str,
    timeout_seconds: u64,
    labels: &HashMap<String, String>,
) -> Result<(Vec<Value>, String)> {
    // Flatten kubectl's shapes: a stream of objects, any of which may itself
    // be a v1 List.
    let mut docs: Vec<Value> = Vec::new();
    for v in converted {
        if v["kind"] == "List" {
            docs.extend(v["items"].as_array().cloned().unwrap_or_default());
        } else {
            docs.push(v);
        }
    }
    if docs.is_empty() {
        return Err(anyhow!("the manifest contains no resources"));
    }

    for doc in &docs {
        let kind = doc["kind"].as_str().unwrap_or("");
        if doc["metadata"]["name"].as_str().unwrap_or("").is_empty() {
            return Err(anyhow!(
                "every resource needs metadata.name (generateName isn't supported — orx \
                 records what it created for cancel/cleanup); a {} is missing one. \
                 Use {{{{ORX_RUN}}}} in names to keep re-runs collision-free.",
                if kind.is_empty() { "resource" } else { kind }
            ));
        }
        if let Some(ns) = doc["metadata"]["namespace"].as_str() {
            if ns != namespace {
                return Err(anyhow!(
                    "{} sets namespace '{}' but runs go to the configured namespace '{}' — \
                     drop metadata.namespace from the manifest.",
                    resource_handle(doc),
                    ns,
                    namespace
                ));
            }
        }
    }

    let job_indices: Vec<usize> = docs
        .iter()
        .enumerate()
        .filter(|(_, d)| d["kind"] == "Job")
        .map(|(i, _)| i)
        .collect();
    let primary = match job_indices.as_slice() {
        [] => {
            return Err(anyhow!(
                "the manifest needs a Job — its completion/failure is the run's outcome"
            ))
        }
        [one] => *one,
        many => {
            let marked: Vec<usize> = many
                .iter()
                .copied()
                .filter(|i| docs[*i]["metadata"]["labels"][PRIMARY_LABEL] == "true")
                .collect();
            match marked.as_slice() {
                [one] => *one,
                _ => {
                    return Err(anyhow!(
                        "the manifest has {} Jobs — label exactly one with `{}: \"true\"` \
                         so orx knows whose completion is the run's",
                        many.len(),
                        PRIMARY_LABEL
                    ))
                }
            }
        }
    };

    let label_map = |v: &mut Value| {
        if !v.is_object() {
            *v = json!({});
        }
        for (k, val) in labels {
            v[k] = json!(val);
        }
    };
    for doc in &mut docs {
        doc["metadata"]["namespace"] = json!(namespace);
        label_map(&mut doc["metadata"]["labels"]);
    }

    let job = &mut docs[primary];
    let job_name = job["metadata"]["name"].as_str().unwrap_or("").to_string();
    label_map(&mut job["spec"]["template"]["metadata"]["labels"]);
    if job["spec"]["activeDeadlineSeconds"].is_null() {
        job["spec"]["activeDeadlineSeconds"] = json!(timeout_seconds);
    }
    if job["spec"]["ttlSecondsAfterFinished"].is_null() {
        job["spec"]["ttlSecondsAfterFinished"] = json!(86400);
    }
    if job["spec"]["backoffLimit"].is_null() {
        // Silent retries would splice two attempts into one run log.
        job["spec"]["backoffLimit"] = json!(0);
    }

    let containers = job["spec"]["template"]["spec"]["containers"]
        .as_array_mut()
        .ok_or_else(|| anyhow!("the Job has no containers"))?;
    let mut references_script = false;
    for c in containers.iter_mut() {
        for field in ["command", "args"] {
            if let Some(items) = c[field].as_array() {
                if items
                    .iter()
                    .any(|a| a.as_str().is_some_and(|s| s.contains("ORX_SCRIPT")))
                {
                    references_script = true;
                }
            }
        }
        let env = c["env"]
            .as_array_mut()
            .map(std::mem::take)
            .unwrap_or_default();
        let mut env: Vec<Value> = env
            .into_iter()
            .filter(|e| e["name"] != "ORX_SCRIPT")
            .collect();
        env.push(json!({ "name": "ORX_SCRIPT", "value": script }));
        c["env"] = json!(env);
        let env_from = c["envFrom"]
            .as_array_mut()
            .map(std::mem::take)
            .unwrap_or_default();
        let mut env_from: Vec<Value> = env_from;
        if !env_from
            .iter()
            .any(|e| e["secretRef"]["name"] == ENV_SECRET)
        {
            env_from.push(json!({ "secretRef": { "name": ENV_SECRET, "optional": true } }));
        }
        c["envFrom"] = json!(env_from);
    }
    if !references_script {
        return Err(anyhow!(
            "no container in Job '{}' runs the experiment: reference the injected script, \
             e.g. command: [\"bash\", \"-c\", \"$ORX_SCRIPT\"] — it clones the branch tip \
             and runs the experiment's fixed run command",
            job_name
        ));
    }

    Ok((docs, job_name))
}

// --- job lifecycle ------------------------------------------------------------

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
    // 1 for plain jobs; the pod count for Indexed jobs.
    let completions = job["spec"]["completions"].as_u64().unwrap_or(1).max(1);

    let empty = Vec::new();
    let conditions = status["conditions"].as_array().unwrap_or(&empty);
    let condition = |ty: &str| -> Option<&Value> {
        conditions
            .iter()
            .find(|c| c["type"] == ty && c["status"] == "True")
    };
    if condition("Complete").is_some() || status["succeeded"].as_u64().unwrap_or(0) >= completions {
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

    // Live: RUNNING once every expected pod runs (the whole gang for Indexed
    // jobs), SCHEDULING before that (surfacing pod waiting reasons like
    // ImagePullBackOff so "stuck" is diagnosable).
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
    let mut live = 0u64;
    for pod in pods["items"].as_array().unwrap_or(&empty) {
        match pod["status"]["phase"].as_str() {
            Some("Running") | Some("Succeeded") => live += 1,
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
    if live >= completions {
        return Ok(JobState {
            stage: "RUNNING".to_string(),
            message: None,
        });
    }
    Ok(JobState {
        stage: "SCHEDULING".to_string(),
        message: waiting_msg,
    })
}

/// Cancel/cleanup = delete the run's recorded resources (`kind/name` handles,
/// newest-first so dependents go before the Job). Missing → already gone.
pub async fn delete_resources(
    context: Option<&str>,
    namespace: &str,
    resources: &[String],
) -> Result<()> {
    for r in resources.iter().rev() {
        match kubectl(
            context,
            &["delete", r, "-n", namespace, "--wait=false"],
            None,
        )
        .await
        {
            Ok(_) => {}
            Err(e) if e.to_string().contains("NotFound") => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// The pod whose stdout is the run log: the completion-index-0 pod for
/// Indexed jobs (the annotation exists on any cluster with Indexed jobs),
/// else the job's first pod by name. `None` while no pod exists yet.
async fn leader_pod(context: Option<&str>, namespace: &str, job_name: &str) -> Option<String> {
    let raw = kubectl(
        context,
        &[
            "get",
            "pods",
            "-n",
            namespace,
            "-l",
            &format!("job-name={job_name}"),
            "-o",
            "json",
        ],
        None,
    )
    .await
    .ok()?;
    let pods: Value = serde_json::from_str(&raw).ok()?;
    let mut names: Vec<&str> = Vec::new();
    for pod in pods["items"].as_array()? {
        let name = pod["metadata"]["name"].as_str()?;
        let index = pod["metadata"]["annotations"]["batch.kubernetes.io/job-completion-index"]
            .as_str()
            .unwrap_or("");
        if index == "0" {
            return Some(name.to_string());
        }
        names.push(name);
    }
    names.sort_unstable();
    names.first().map(|n| n.to_string())
}

/// One pass over `kubectl logs -f` on the job's leader pod, invoking `sink`
/// per line past `skip`.
///
/// Same replay/dedup contract as `hf::stream_logs`: kubectl replays the log
/// from the start on each (re)connect, so the caller passes how many lines it
/// has consumed and gets the new total back. Ends when kubectl exits (pod
/// gone/finished) or after `idle` silence; the supervisor re-checks job state
/// and reconnects if still live.
///
/// Only the leader pod is captured: a stable single stream keeps the
/// line-count dedup sound (interleaving N pods would reorder across
/// reconnects), and the leader is where the driver's output lives. Other
/// pods stay reachable via `kubectl logs`.
pub async fn stream_logs(
    context: Option<&str>,
    namespace: &str,
    job_name: &str,
    skip: u64,
    idle: Duration,
    sink: &mut (dyn FnMut(&str) + Send),
) -> Result<u64> {
    use tokio::io::{AsyncBufReadExt as _, BufReader};

    let target = match leader_pod(context, namespace, job_name).await {
        Some(pod) => format!("pod/{pod}"),
        // Not scheduled yet — let the supervisor's retry loop come back.
        None => return Ok(skip),
    };

    let mut cmd = Command::new("kubectl");
    if let Some(ctx) = context {
        cmd.arg("--context").arg(ctx);
    }
    let mut child = cmd
        .args(["logs", "-f", &target, "-n", namespace, "--tail=-1"])
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

#[cfg(test)]
mod tests {
    use super::*;

    fn job(name: &str) -> Value {
        json!({
            "apiVersion": "batch/v1",
            "kind": "Job",
            "metadata": { "name": name },
            "spec": {
                "template": {
                    "spec": {
                        "restartPolicy": "Never",
                        "containers": [{
                            "name": "run",
                            "image": "python:3.12",
                            "command": ["bash", "-c", "$ORX_SCRIPT"],
                        }],
                    },
                },
            },
        })
    }

    fn labels() -> HashMap<String, String> {
        HashMap::from([("or_run".to_string(), "r1".to_string())])
    }

    fn prepare(v: Value) -> Result<(Vec<Value>, String)> {
        prepare_docs(vec![v], "default", "echo hi", 14400, &labels())
    }

    #[test]
    fn single_job_gets_defaults_labels_script_and_secret() {
        let (docs, name) = prepare(job("train-r1")).unwrap();
        assert_eq!(name, "train-r1");
        let j = &docs[0];
        assert_eq!(j["metadata"]["namespace"], "default");
        assert_eq!(j["metadata"]["labels"]["or_run"], "r1");
        assert_eq!(j["spec"]["template"]["metadata"]["labels"]["or_run"], "r1");
        assert_eq!(j["spec"]["activeDeadlineSeconds"], 14400);
        assert_eq!(j["spec"]["ttlSecondsAfterFinished"], 86400);
        assert_eq!(j["spec"]["backoffLimit"], 0);
        let c = &j["spec"]["template"]["spec"]["containers"][0];
        assert_eq!(c["env"][0]["name"], "ORX_SCRIPT");
        assert_eq!(c["env"][0]["value"], "echo hi");
        assert_eq!(c["envFrom"][0]["secretRef"]["name"], ENV_SECRET);
    }

    #[test]
    fn author_settings_win_over_defaults() {
        let mut j = job("train");
        j["spec"]["activeDeadlineSeconds"] = json!(60);
        j["spec"]["backoffLimit"] = json!(2);
        let (docs, _) = prepare(j).unwrap();
        assert_eq!(docs[0]["spec"]["activeDeadlineSeconds"], 60);
        assert_eq!(docs[0]["spec"]["backoffLimit"], 2);
    }

    #[test]
    fn list_with_aux_service_keeps_order_and_finds_the_job() {
        let svc = json!({
            "apiVersion": "v1", "kind": "Service",
            "metadata": { "name": "rendezvous" },
            "spec": { "clusterIP": "None" },
        });
        let list = json!({ "kind": "List", "items": [svc, job("train")] });
        let (docs, name) = prepare(list).unwrap();
        assert_eq!(name, "train");
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0]["kind"], "Service");
        assert_eq!(docs[0]["metadata"]["labels"]["or_run"], "r1");
    }

    #[test]
    fn multi_doc_stream_as_separate_values_works() {
        // kubectl -o json emits concatenated objects for multi-doc input.
        let svc = json!({ "kind": "Service", "metadata": { "name": "rendezvous" } });
        let (docs, name) = prepare_docs(
            vec![svc, job("train")],
            "default",
            "echo hi",
            14400,
            &labels(),
        )
        .unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(name, "train");
    }

    #[test]
    fn no_job_is_an_error() {
        let svc = json!({ "kind": "Service", "metadata": { "name": "s" } });
        assert!(prepare(svc)
            .unwrap_err()
            .to_string()
            .contains("needs a Job"));
    }

    #[test]
    fn two_jobs_need_a_primary_label() {
        let list = json!({ "kind": "List", "items": [job("a"), job("b")] });
        assert!(prepare(list)
            .unwrap_err()
            .to_string()
            .contains("orx-primary"));

        let mut marked = job("b");
        marked["metadata"]["labels"] = json!({ "orx-primary": "true" });
        let list = json!({ "kind": "List", "items": [job("a"), marked] });
        let (_, name) = prepare(list).unwrap();
        assert_eq!(name, "b");
    }

    #[test]
    fn job_must_reference_the_script() {
        let mut j = job("train");
        j["spec"]["template"]["spec"]["containers"][0]["command"] = json!(["python", "train.py"]);
        assert!(prepare(j).unwrap_err().to_string().contains("ORX_SCRIPT"));
    }

    #[test]
    fn generate_name_and_foreign_namespace_are_errors() {
        let mut j = job("");
        j["metadata"] = json!({ "generateName": "train-" });
        assert!(prepare(j)
            .unwrap_err()
            .to_string()
            .contains("metadata.name"));

        let mut j = job("train");
        j["metadata"]["namespace"] = json!("other");
        assert!(prepare(j).unwrap_err().to_string().contains("namespace"));
    }

    #[test]
    fn author_orx_script_env_is_replaced_and_secret_not_duplicated() {
        let mut j = job("train");
        j["spec"]["template"]["spec"]["containers"][0]["env"] =
            json!([{ "name": "ORX_SCRIPT", "value": "evil" }, { "name": "FOO", "value": "1" }]);
        j["spec"]["template"]["spec"]["containers"][0]["envFrom"] =
            json!([{ "secretRef": { "name": ENV_SECRET } }]);
        let (docs, _) = prepare(j).unwrap();
        let c = &docs[0]["spec"]["template"]["spec"]["containers"][0];
        let env = c["env"].as_array().unwrap();
        assert_eq!(env.len(), 2);
        assert!(env.iter().any(|e| e["name"] == "FOO"));
        assert!(env
            .iter()
            .any(|e| e["name"] == "ORX_SCRIPT" && e["value"] == "echo hi"));
        assert_eq!(c["envFrom"].as_array().unwrap().len(), 1);
    }
}
