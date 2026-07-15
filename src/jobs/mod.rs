//! Job backends — external compute that orx launches and supervises itself.
//!
//! The api never orchestrates these: orx submits natively (HF Jobs, Modal,
//! Kubernetes, SSH, Slurm, an OpenResearch box, this machine), a detached
//! `orx supervise` watches the job beside it, and the api receives status/log
//! mirrors. The run's `backend_json` descriptor is the serialized handle a
//! later supervisor uses to reattach.

pub mod huggingface;
pub mod kubernetes;
pub mod localbox;
pub mod modal;
pub mod openresearch;
pub mod slurm;
pub mod ssh;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error::{anyhow, Result};

/// CPython env var that forces stdout/stderr unbuffered. Every backend streams a
/// job's output by tailing a pipe or a redirected `log` file, so a block-buffered
/// job would make its logs appear frozen until the buffer fills. We default it on
/// so prints stream live; it's inert for non-Python jobs and inherited by child
/// processes (unlike the `-u` flag).
pub const PYTHONUNBUFFERED: &str = "PYTHONUNBUFFERED";

/// Default `PYTHONUNBUFFERED=1` into a job's environment map unless the caller
/// already set it (an explicit value always wins). Shared by every backend that
/// carries env as a `HashMap`; kubernetes open-codes the equivalent because its
/// env is a JSON `[{name, value}]` array, not a map.
pub fn default_unbuffered(env: &HashMap<String, String>) -> HashMap<String, String> {
    let mut env = env.clone();
    env.entry(PYTHONUNBUFFERED.to_string())
        .or_insert_with(|| "1".to_string());
    env
}

/// Backend descriptor stored on the run (locally and mirrored to the api).
/// `kind` discriminates. This is a fixed field list — a key absent here does
/// NOT survive a parse → to_json round-trip, so anything a backend must keep
/// needs its own (optional) field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackendDescriptor {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flavor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// kubeconfig context (k8s_job only); `None` = kubectl current-context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// Repo-relative manifest path the run was launched from (k8s_job only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<String>,
    /// Everything the manifest created, as `kind/name` handles in creation
    /// order (k8s_job only) — cancel deletes exactly this list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Vec<String>>,
    /// The box's SSH endpoint (openresearch_job only), recorded by the
    /// supervisor once provisioning finishes — `None` while the box boots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_port: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_user: Option<String>,
    /// Wall-clock bound the supervisor wraps around the payload
    /// (openresearch_job only) — persisted here because the launch happens in
    /// the supervisor, long after the `--timeout` flag is gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

impl BackendDescriptor {
    pub fn parse(json: &str) -> Result<Self> {
        serde_json::from_str(json).map_err(|e| anyhow!("Unreadable backend descriptor: {}", e))
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// The HF (namespace, jobId) handle, or an error naming what's missing.
    pub fn hf_ref(&self) -> Result<(&str, &str)> {
        if self.kind != "hf_job" {
            return Err(anyhow!("Unsupported backend kind: {}", self.kind));
        }
        match (self.namespace.as_deref(), self.job_id.as_deref()) {
            (Some(ns), Some(id)) => Ok((ns, id)),
            _ => Err(anyhow!(
                "Backend descriptor is missing namespace/jobId — was the job submitted?"
            )),
        }
    }

    /// The k8s (namespace, job name) handle; context rides on `self.context`.
    pub fn k8s_ref(&self) -> Result<(&str, &str)> {
        if self.kind != "k8s_job" {
            return Err(anyhow!("Unsupported backend kind: {}", self.kind));
        }
        match (self.namespace.as_deref(), self.job_id.as_deref()) {
            (Some(ns), Some(id)) => Ok((ns, id)),
            _ => Err(anyhow!(
                "Backend descriptor is missing namespace/jobName — was the job submitted?"
            )),
        }
    }

    /// The Modal sandbox id (the reattach handle); `namespace` holds the app.
    pub fn modal_ref(&self) -> Result<&str> {
        if self.kind != "modal_job" {
            return Err(anyhow!("Unsupported backend kind: {}", self.kind));
        }
        self.job_id.as_deref().ok_or_else(|| {
            anyhow!("Backend descriptor is missing the Modal sandbox id — was the job submitted?")
        })
    }

    /// The Slurm (host, job id) handle; the login-node host rides on
    /// `namespace`, and the run dir derives from the run id.
    pub fn slurm_ref(&self) -> Result<(&str, &str)> {
        if self.kind != "slurm_job" {
            return Err(anyhow!("Unsupported backend kind: {}", self.kind));
        }
        match (self.namespace.as_deref(), self.job_id.as_deref()) {
            (Some(host), Some(id)) => Ok((host, id)),
            _ => Err(anyhow!(
                "Backend descriptor is missing the slurm host/job id — was the job submitted?"
            )),
        }
    }

    /// The local run dir (the reattach handle) for an on-this-machine run.
    pub fn local_ref(&self) -> Result<&str> {
        if self.kind != "local_job" {
            return Err(anyhow!("Unsupported backend kind: {}", self.kind));
        }
        self.job_id.as_deref().ok_or_else(|| {
            anyhow!("Backend descriptor is missing the run dir — was the job submitted?")
        })
    }

    /// The SSH (host, remote run dir) handle; host rides on `namespace`.
    pub fn ssh_ref(&self) -> Result<(&str, &str)> {
        if self.kind != "ssh_job" {
            return Err(anyhow!("Unsupported backend kind: {}", self.kind));
        }
        match (self.namespace.as_deref(), self.job_id.as_deref()) {
            (Some(host), Some(dir)) => Ok((host, dir)),
            _ => Err(anyhow!(
                "Backend descriptor is missing the ssh host/dir — was the job submitted?"
            )),
        }
    }

    /// The OpenResearch (org id, sandbox id) handle; the org rides on
    /// `namespace`, and the run dir derives from the run id.
    pub fn openresearch_ref(&self) -> Result<(&str, &str)> {
        if self.kind != "openresearch_job" {
            return Err(anyhow!("Unsupported backend kind: {}", self.kind));
        }
        match (self.namespace.as_deref(), self.job_id.as_deref()) {
            (Some(org), Some(id)) => Ok((org, id)),
            _ => Err(anyhow!(
                "Backend descriptor is missing the org/sandbox id — was the box provisioned?"
            )),
        }
    }

    /// The box's SSH endpoint as a ready-to-use target, once the supervisor
    /// has recorded it (openresearch_job only). Host keys are accepted on
    /// first use: the box is freshly provisioned and providers recycle proxy
    /// host:port pairs, so pinning would only produce false mismatches — the
    /// platform's own ssh access to these boxes behaves the same way.
    pub fn openresearch_ssh_target(&self) -> Option<ssh::SshTarget> {
        if self.kind != "openresearch_job" {
            return None;
        }
        let (host, port, user) = (
            self.ssh_host.as_deref()?,
            self.ssh_port?,
            self.ssh_user.as_deref()?,
        );
        Some(ssh::SshTarget {
            dest: format!("{user}@{host}"),
            extra_opts: vec![
                "-p".into(),
                port.to_string(),
                "-o".into(),
                "StrictHostKeyChecking=no".into(),
                "-o".into(),
                "UserKnownHostsFile=/dev/null".into(),
                "-o".into(),
                "LogLevel=ERROR".into(),
            ],
        })
    }
}

/// Map an HF job stage onto the run-status vocabulary the store and the api
/// share. `UPDATING` appears in the wild as a live state (see huggingface_hub).
pub fn stage_to_run_status(stage: &str) -> &'static str {
    match stage {
        "SCHEDULING" => "starting",
        "RUNNING" | "UPDATING" => "running",
        "COMPLETED" => "done",
        "ERROR" => "failed",
        "CANCELED" | "DELETED" => "cancelled",
        _ => "running",
    }
}

pub fn is_terminal_stage(stage: &str) -> bool {
    matches!(stage, "COMPLETED" | "CANCELED" | "ERROR" | "DELETED")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn openresearch_descriptor() -> BackendDescriptor {
        BackendDescriptor {
            kind: "openresearch_job".to_string(),
            namespace: Some("org_1".to_string()),
            job_id: Some("sb_1".to_string()),
            flavor: Some("h100_sxm:2".to_string()),
            image: None,
            url: None,
            context: None,
            manifest: None,
            resources: None,
            ssh_host: None,
            ssh_port: None,
            ssh_user: None,
            timeout_secs: Some(14_400),
        }
    }

    /// The ssh endpoint fields are camelCase on the wire, absent while `None`,
    /// and survive a parse → to_json round-trip once set.
    #[test]
    fn openresearch_descriptor_round_trips() {
        let mut d = openresearch_descriptor();
        let json = d.to_json();
        assert!(
            !json.contains("sshHost"),
            "None fields must not serialize: {json}"
        );
        assert!(json.contains("\"timeoutSecs\":14400"), "{json}");

        d.ssh_host = Some("203.0.113.7".to_string());
        d.ssh_port = Some(22022);
        d.ssh_user = Some("root".to_string());
        let json = d.to_json();
        assert!(json.contains("\"sshHost\":\"203.0.113.7\""), "{json}");

        let back = BackendDescriptor::parse(&json).unwrap();
        assert_eq!(back.ssh_port, Some(22022));
        assert_eq!(back.openresearch_ref().unwrap(), ("org_1", "sb_1"));
    }

    /// Descriptors written before the ssh/timeout fields existed still parse.
    #[test]
    fn older_descriptors_without_new_fields_still_parse() {
        let d = BackendDescriptor::parse(
            r#"{"kind":"ssh_job","namespace":"mybox","jobId":".orx/runs/r1"}"#,
        )
        .unwrap();
        assert_eq!(d.ssh_ref().unwrap(), ("mybox", ".orx/runs/r1"));
        assert_eq!(d.ssh_host, None);
        assert_eq!(d.timeout_secs, None);
    }

    #[test]
    fn openresearch_ref_rejects_other_kinds() {
        let mut d = openresearch_descriptor();
        d.kind = "ssh_job".to_string();
        assert!(d.openresearch_ref().is_err());
        assert!(d.openresearch_ssh_target().is_none());
    }

    /// The target is only available once the supervisor recorded the endpoint,
    /// and carries the port + first-use host-key options.
    #[test]
    fn openresearch_ssh_target_builds_dest_and_opts() {
        let mut d = openresearch_descriptor();
        assert!(d.openresearch_ssh_target().is_none());
        d.ssh_host = Some("203.0.113.7".to_string());
        d.ssh_port = Some(22022);
        d.ssh_user = Some("root".to_string());
        let target = d.openresearch_ssh_target().unwrap();
        assert_eq!(target.dest, "root@203.0.113.7");
        let opts = target.extra_opts.join(" ");
        assert!(opts.contains("-p 22022"), "{opts}");
        assert!(opts.contains("StrictHostKeyChecking=no"), "{opts}");
    }

    #[test]
    fn default_unbuffered_injects_when_absent_and_lets_author_win() {
        // Injected when the caller didn't set it.
        let got = default_unbuffered(&HashMap::new());
        assert_eq!(got.get(PYTHONUNBUFFERED).map(String::as_str), Some("1"));

        // An explicit value is preserved — even a falsy one — never overwritten.
        let author = HashMap::from([(PYTHONUNBUFFERED.to_string(), "0".to_string())]);
        let got = default_unbuffered(&author);
        assert_eq!(got.get(PYTHONUNBUFFERED).map(String::as_str), Some("0"));
    }
}
