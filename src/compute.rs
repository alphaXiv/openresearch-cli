//! Backend-agnostic compute lifecycle and immutable source snapshots.
//!
//! Local Git remains the experiment-history database. A launch never asks a
//! remote backend to clone that history: it archives the exact recorded commit
//! once, addresses the archive by SHA-256, and hands that immutable payload to
//! the selected provider adapter.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use async_trait::async_trait;
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::error::{anyhow, Result};
use crate::jobs::BackendDescriptor;
use crate::local::model::{LocalExperiment, LocalProject};
use crate::store::{log_path, Store, StoredRun};

#[derive(Debug, Clone)]
pub struct SourceSnapshot {
    pub revision: String,
    pub digest: String,
    pub size: u64,
    pub path: PathBuf,
    pub ray_package: Option<(String, PathBuf)>,
}

impl SourceSnapshot {
    pub fn create(
        project: &LocalProject,
        experiment: &LocalExperiment,
        include_ray_package: bool,
    ) -> Result<Self> {
        let repo = Path::new(&project.repo_path);
        let revision = crate::local::git::local_head_sha(repo, &experiment.branch_name)?;
        let dir = crate::store::data_dir().join("source-snapshots");
        std::fs::create_dir_all(&dir)?;

        let nonce = uuid::Uuid::new_v4();
        let tar_tmp = dir.join(format!(".{nonce}.tar"));
        archive(repo, &revision, "tar", &tar_tmp)?;
        let (digest, size) = digest_file(&tar_tmp)?;
        let path = dir.join(format!("{digest}.tar"));
        install_content_addressed(&tar_tmp, &path, &digest, size)?;

        let ray_package = if include_ray_package {
            let zip_tmp = dir.join(format!(".{nonce}.zip"));
            archive(repo, &revision, "zip", &zip_tmp)?;
            let (zip_digest, zip_size) = digest_file(&zip_tmp)?;
            let zip_path = dir.join(format!("{zip_digest}.zip"));
            install_content_addressed(&zip_tmp, &zip_path, &zip_digest, zip_size)?;
            Some((zip_digest, zip_path))
        } else {
            None
        };

        Ok(Self {
            revision,
            digest,
            size,
            path,
            ray_package,
        })
    }

    pub fn apply_to_descriptor(&self, descriptor: &mut BackendDescriptor) {
        descriptor.source_digest = Some(self.digest.clone());
        descriptor.source_path = Some(self.path.to_string_lossy().into_owned());
        descriptor.source_size = Some(self.size);
    }

    pub fn from_run(run: &StoredRun, descriptor: &BackendDescriptor) -> Result<Self> {
        let revision = run
            .commit_sha
            .clone()
            .ok_or_else(|| anyhow!("Run {} has no recorded source revision.", run.id))?;
        let digest = descriptor
            .source_digest
            .clone()
            .ok_or_else(|| anyhow!("Run {} has no recorded source digest.", run.id))?;
        let recorded_path = descriptor
            .source_path
            .as_deref()
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("Run {} has no recorded source archive.", run.id))?;
        let path = if recorded_path.is_file() {
            recorded_path
        } else {
            crate::store::data_dir()
                .join("source-snapshots")
                .join(format!("{digest}.tar"))
        };
        if !path.is_file() {
            return Err(anyhow!(
                "Run {} source archive is missing at {}.",
                run.id,
                path.display()
            ));
        }
        let size = descriptor
            .source_size
            .or_else(|| std::fs::metadata(&path).ok().map(|m| m.len()))
            .unwrap_or(0);
        let (actual_digest, actual_size) = digest_file(&path)?;
        if actual_digest != digest || (size != 0 && actual_size != size) {
            return Err(anyhow!(
                "Run {} source archive failed its digest check.",
                run.id
            ));
        }
        Ok(Self {
            revision,
            digest,
            size,
            path,
            ray_package: None,
        })
    }
}

fn archive(repo: &Path, revision: &str, format: &str, destination: &Path) -> Result<()> {
    let file = std::fs::File::create(destination)?;
    let output = Command::new("git")
        .current_dir(repo)
        .args(["archive", &format!("--format={format}"), revision])
        .stdout(Stdio::from(file))
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| anyhow!("Could not run git archive: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let _ = std::fs::remove_file(destination);
    Err(anyhow!(
        "git archive failed for {}: {}",
        revision,
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

fn digest_file(path: &Path) -> Result<(String, u64)> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buf = [0u8; 128 * 1024];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        size += read as u64;
    }
    Ok((format!("{:x}", hasher.finalize()), size))
}

fn install_content_addressed(
    source: &Path,
    destination: &Path,
    expected_digest: &str,
    expected_size: u64,
) -> Result<()> {
    if destination.exists() {
        let (digest, size) = digest_file(destination)?;
        if digest == expected_digest && size == expected_size {
            std::fs::remove_file(source)?;
            return Ok(());
        }
        std::fs::remove_file(destination)?;
    }
    match std::fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(_err) if destination.exists() => {
            let (digest, size) = digest_file(destination)?;
            std::fs::remove_file(source)?;
            if digest == expected_digest && size == expected_size {
                Ok(())
            } else {
                Err(anyhow!(
                    "Cached source snapshot {} failed its digest check.",
                    destination.display()
                ))
            }
        }
        Err(err) => Err(err.into()),
    }
}

pub fn snapshot_script(archive_path: &str, command: &str) -> String {
    format!(
        "set -eo pipefail; mkdir -p repo; tar -xf {} -C repo; cd repo; {}",
        shell_quote(archive_path),
        command
    )
}

pub fn staged_script(command: &str) -> String {
    format!("set -eo pipefail; cd repo; {command}")
}

pub fn gated_script(archive_path: &str, command: &str) -> String {
    format!(
        "set -eo pipefail; mkdir -p \"$(dirname -- {archive})\"; while [ ! -f {ready} ]; do sleep 0.1; done; mkdir -p repo; tar -xf {archive} -C repo; cd repo; {command}",
        archive = shell_quote(archive_path),
        ready = shell_quote(&format!("{archive_path}.ready")),
    )
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    pub id: &'static str,
    pub label: &'static str,
    pub remote: bool,
    pub flavors: bool,
    pub requires_flavor: bool,
    pub source_transport: &'static str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preflight {
    pub ready: bool,
    pub detail: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StagedSource(pub SourceSnapshot);

#[derive(Debug, Clone, Copy, Default)]
pub struct LogCursor(pub u64);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogBatch {
    pub data_base64: String,
    pub next_cursor: u64,
    pub eof: bool,
}

#[async_trait]
pub trait ComputeBackend: Send + Sync {
    fn capabilities(&self) -> Capabilities;
    async fn preflight(&self, args: &crate::ExpRunArgs) -> Result<Preflight>;
    async fn stage_source(
        &self,
        project: &LocalProject,
        experiment: &LocalExperiment,
    ) -> Result<StagedSource>;
    async fn submit(
        &self,
        args: &crate::ExpRunArgs,
        source: StagedSource,
        run_id: String,
    ) -> Result<StoredRun>;

    async fn status(&self, handle: &StoredRun) -> Result<StoredRun> {
        Store::open()?
            .get_run(&handle.id)?
            .ok_or_else(|| anyhow!("Run {} not found.", handle.id))
    }

    async fn logs(&self, handle: &StoredRun, cursor: LogCursor) -> Result<LogBatch> {
        use base64::Engine as _;
        use std::io::{Read as _, Seek as _};
        let path = log_path(&handle.id);
        let mut file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(_) => {
                return Ok(LogBatch {
                    data_base64: String::new(),
                    next_cursor: cursor.0,
                    eof: crate::local::is_terminal(&handle.status),
                })
            }
        };
        let len = file.metadata()?.len();
        let start = cursor.0.min(len);
        file.seek(std::io::SeekFrom::Start(start))?;
        let mut data = Vec::new();
        file.take(256 * 1024).read_to_end(&mut data)?;
        let next = start + data.len() as u64;
        Ok(LogBatch {
            data_base64: base64::engine::general_purpose::STANDARD.encode(data),
            next_cursor: next,
            eof: crate::local::is_terminal(&handle.status) && next >= len,
        })
    }

    async fn cancel(&self, handle: &StoredRun) -> Result<()> {
        crate::commands::exp::request_local_run_cancel(&Store::open()?, &handle.id)
    }

    async fn cleanup(&self, handle: &StoredRun) -> Result<()> {
        if self.capabilities().id == "hf" && crate::local::is_terminal(&handle.status) {
            let descriptor = BackendDescriptor::parse(&handle.backend_json)?;
            if let (Some(path), Some(digest)) = (descriptor.source_path, descriptor.source_digest) {
                let staging = Path::new(&path)
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(format!("{digest}.hf"));
                if staging.is_dir() {
                    std::fs::remove_dir_all(staging)?;
                }
            }
        }
        Ok(())
    }
}

pub struct LocalBackend {
    id: &'static str,
}

impl LocalBackend {
    pub fn named(id: &str) -> Result<Self> {
        let id = crate::local::BACKENDS
            .iter()
            .copied()
            .find(|candidate| *candidate == id)
            .ok_or_else(|| anyhow!("Unknown compute backend '{id}'."))?;
        Ok(Self { id })
    }
}

#[async_trait]
impl ComputeBackend for LocalBackend {
    fn capabilities(&self) -> Capabilities {
        let (label, transport) = match self.id {
            "local" => ("This machine", "local archive"),
            "hf" => ("Hugging Face Jobs", "private job volume"),
            "modal" => ("Modal", "sandbox filesystem"),
            "k8s" => ("Kubernetes", "kubectl cp"),
            "ssh" => ("SSH", "SSH tar stream"),
            "slurm" => ("Slurm", "SSH tar stream"),
            "ray" => ("Ray Jobs", "working_dir package"),
            "openresearch" => ("OpenResearch", "SSH tar stream"),
            _ => unreachable!(),
        };
        Capabilities {
            id: self.id,
            label,
            remote: self.id != "local",
            flavors: crate::local::FLAVORED_BACKENDS.contains(&self.id),
            requires_flavor: crate::local::FLAVOR_REQUIRED_BACKENDS.contains(&self.id),
            source_transport: transport,
        }
    }

    async fn preflight(&self, args: &crate::ExpRunArgs) -> Result<Preflight> {
        let detail = match self.id {
            "local" => None,
            "hf" => {
                if args.flavor.is_none() {
                    return Ok(not_ready("Hugging Face Jobs requires --flavor."));
                }
                let token = crate::jobs::huggingface::resolve_token()?;
                crate::jobs::huggingface::whoami(&token).await?;
                None
            }
            "modal" => {
                if args.flavor.is_none() {
                    return Ok(not_ready("Modal requires --flavor."));
                }
                crate::jobs::modal::preflight().await?;
                None
            }
            "k8s" => {
                let settings = crate::jobs::kubernetes::load_settings()?.unwrap_or_default();
                let check = crate::jobs::kubernetes::preflight(
                    settings.context.as_deref(),
                    &settings.namespace,
                )
                .await;
                if !check.kubectl_found || !check.reachable || !check.can_create_jobs {
                    return Ok(not_ready(check.error.as_deref().unwrap_or(
                        "kubectl cannot reach the cluster or create Jobs in the namespace.",
                    )));
                }
                None
            }
            "ssh" => {
                let host = args
                    .host
                    .as_deref()
                    .ok_or_else(|| anyhow!("SSH requires --host <alias>."))?;
                let check =
                    crate::jobs::ssh::preflight(&crate::jobs::ssh::SshTarget::alias(host)).await;
                if !check.reachable || !check.tools_found {
                    return Ok(not_ready(
                        check
                            .error
                            .as_deref()
                            .unwrap_or("The SSH host needs bash and tar."),
                    ));
                }
                None
            }
            "slurm" => {
                let settings = crate::jobs::slurm::load_settings()?.unwrap_or_default();
                let host = args
                    .host
                    .as_deref()
                    .or(settings.host.as_deref())
                    .ok_or_else(|| anyhow!("Slurm requires --host or a configured host."))?;
                let check = crate::jobs::slurm::preflight(host).await;
                if !check.reachable || !check.slurm_found || !check.tools_found {
                    return Ok(not_ready(check.error.as_deref().unwrap_or(
                        "The Slurm host needs bash, tar, sbatch, squeue, and scancel.",
                    )));
                }
                None
            }
            "ray" => {
                let address = crate::jobs::ray::resolve_address(None);
                crate::jobs::ray::preflight(&address).await?;
                None
            }
            "openresearch" => {
                if args.flavor.is_none() {
                    return Ok(not_ready("OpenResearch requires --flavor."));
                }
                if crate::config::load_credentials().await?.is_none() {
                    return Ok(not_ready("OpenResearch requires `orx login`."));
                }
                None
            }
            _ => unreachable!(),
        };
        Ok(Preflight {
            ready: true,
            detail,
        })
    }

    async fn stage_source(
        &self,
        project: &LocalProject,
        experiment: &LocalExperiment,
    ) -> Result<StagedSource> {
        let project = project.clone();
        let experiment = experiment.clone();
        let include_ray_package = self.id == "ray";
        tokio::task::spawn_blocking(move || {
            SourceSnapshot::create(&project, &experiment, include_ray_package)
        })
        .await
        .map_err(|e| anyhow!("source snapshot task failed: {e}"))?
        .map(StagedSource)
    }

    async fn submit(
        &self,
        args: &crate::ExpRunArgs,
        source: StagedSource,
        run_id: String,
    ) -> Result<StoredRun> {
        match self.id {
            "local" => {
                crate::local::localrun::submit_local_run_with_source(args, source.0, run_id).await
            }
            "hf" => crate::local::hf::submit_local_hf_with_source(args, source.0, run_id).await,
            "modal" => {
                crate::local::modal::submit_local_modal_with_source(args, source.0, run_id).await
            }
            "k8s" => crate::local::k8s::submit_local_k8s_with_source(args, source.0, run_id).await,
            "ssh" => crate::local::ssh::submit_local_ssh_with_source(args, source.0, run_id).await,
            "slurm" => {
                crate::local::slurm::submit_local_slurm_with_source(args, source.0, run_id).await
            }
            "ray" => crate::local::ray::submit_local_ray_with_source(args, source.0, run_id).await,
            "openresearch" => {
                crate::local::openresearch::submit_local_openresearch_with_source(
                    args, source.0, run_id,
                )
                .await
            }
            _ => unreachable!(),
        }
    }
}

pub fn capabilities() -> Vec<Capabilities> {
    crate::local::BACKENDS
        .iter()
        .filter_map(|id| LocalBackend::named(id).ok())
        .map(|backend| backend.capabilities())
        .collect()
}

pub async fn submit(args: &crate::ExpRunArgs) -> Result<StoredRun> {
    let backend_id = args.backend.as_deref().unwrap_or("local");
    let backend = LocalBackend::named(backend_id)?;
    let store = Store::open()?;
    let experiment = store
        .get_local_experiment(&args.exp_id)?
        .ok_or_else(|| anyhow!("Local experiment {} not found.", args.exp_id))?;
    let project = store
        .get_local_project(&experiment.project_id)?
        .ok_or_else(|| anyhow!("Local project {} not found.", experiment.project_id))?;
    let preflight = backend.preflight(args).await?;
    if !preflight.ready {
        return Err(anyhow!(
            "{}",
            preflight
                .detail
                .unwrap_or_else(|| "Compute backend is not ready.".to_string())
        ));
    }
    let source = backend.stage_source(&project, &experiment).await?;
    let run_id = uuid::Uuid::new_v4().to_string();
    let command = Some(experiment.run_command.clone())
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            project
                .run_command
                .clone()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_default();
    let mut descriptor = BackendDescriptor {
        kind: format!("{}_job", backend_id),
        namespace: None,
        job_id: None,
        flavor: args.flavor.clone(),
        image: args.image.clone(),
        url: None,
        context: None,
        manifest: args.manifest.clone(),
        resources: None,
        ssh_host: None,
        ssh_port: None,
        ssh_user: None,
        timeout_secs: None,
        source_digest: None,
        source_path: None,
        source_size: None,
    };
    source.0.apply_to_descriptor(&mut descriptor);
    let now = crate::store::now_ms();
    let pending = StoredRun {
        id: run_id.clone(),
        experiment_id: experiment.id.clone(),
        project_id: project.id.clone(),
        status: "starting".to_string(),
        backend_json: descriptor.to_json(),
        command,
        created_at: now,
        updated_at: now,
        ended_at: None,
        exit_code: None,
        commit_sha: Some(source.0.revision.clone()),
        result_markdown: None,
        cancel_requested: false,
        chat_session_id: crate::local::chat::launching_chat_session(),
    };
    reserve_run(&store, &pending, args.force)?;
    let pending_backend_json = descriptor.to_json();
    match backend.submit(args, source, run_id.clone()).await {
        Ok(run) => Ok(run),
        Err(error) => {
            let current = store.get_run(&run_id)?;
            let handle_was_persisted = current
                .as_ref()
                .is_some_and(|run| run.backend_json != pending_backend_json);
            if !handle_was_persisted {
                store.update_status(&run_id, "failed", Some(crate::store::now_ms()), None)?;
                store
                    .set_result_markdown(&run_id, &format!("Compute submission failed: {error}"))?;
            }
            Err(error)
        }
    }
}

fn reserve_run(store: &Store, pending: &StoredRun, force: bool) -> Result<()> {
    let dir = crate::store::data_dir().join("submission-locks");
    std::fs::create_dir_all(&dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(dir.join(&pending.experiment_id))?;
    let mut lock = fd_lock::RwLock::new(file);
    let _guard = lock.write()?;
    if !force {
        if let Some(run) = store
            .list_runs_by_experiment(&pending.experiment_id)?
            .into_iter()
            .find(|run| !crate::local::is_terminal(&run.status))
        {
            return Err(anyhow!(
                "Run {} is already in flight for this experiment ({}). Cancel it with \
                 `orx exp cancel {}` or pass --force to launch anyway.",
                run.id,
                run.status,
                pending.experiment_id
            ));
        }
    }
    store.upsert_run(pending)
}

pub fn record_submission_handle(run_id: &str, descriptor: &BackendDescriptor) -> Result<()> {
    let database_error = Store::open()
        .and_then(|store| store.set_backend_json(run_id, &descriptor.to_json()))
        .err();
    let dir = crate::store::data_dir().join("submission-handles");
    if let Err(error) = std::fs::create_dir_all(&dir) {
        return match database_error {
            None => {
                eprintln!("warning: could not create submission recovery directory: {error}");
                Ok(())
            }
            Some(database_error) => Err(anyhow!(
                "Could not persist provider handle in SQLite ({database_error}) or the recovery directory ({error})."
            )),
        };
    }
    let destination = dir.join(format!("{run_id}.json"));
    let temporary = dir.join(format!(".{run_id}.{}.tmp", uuid::Uuid::new_v4()));
    let file_error = std::fs::write(&temporary, descriptor.to_json())
        .and_then(|()| std::fs::rename(&temporary, destination))
        .err();
    if let Some(error) = file_error {
        let _ = std::fs::remove_file(temporary);
        if let Some(database_error) = database_error {
            return Err(anyhow!(
                "Could not persist provider handle in SQLite ({database_error}) or its recovery file ({error})."
            ));
        }
        eprintln!("warning: could not write redundant submission recovery record: {error}");
    }
    Ok(())
}

pub fn recover_submission_handle(run_id: &str) -> Result<Option<BackendDescriptor>> {
    let path = crate::store::data_dir()
        .join("submission-handles")
        .join(format!("{run_id}.json"));
    match std::fs::read_to_string(path) {
        Ok(json) => BackendDescriptor::parse(&json).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn not_ready(detail: impl Into<String>) -> Preflight {
    Preflight {
        ready: false,
        detail: Some(detail.into()),
    }
}
