//! Job backends — external compute that orx launches and supervises itself.
//!
//! The api never orchestrates these: orx submits natively (HF Jobs today,
//! SLURM/local later), a detached `orx supervise` watches the job beside it,
//! and the api receives status/log mirrors. The run's `backend_json`
//! descriptor is the serialized handle a later supervisor uses to reattach.

pub mod huggingface;
pub mod kubernetes;
pub mod modal;
pub mod ssh;

use serde::{Deserialize, Serialize};

use crate::error::{anyhow, Result};

/// Backend descriptor stored on the run (locally and mirrored to the api).
/// `kind` discriminates; unknown fields ride along untouched.
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
