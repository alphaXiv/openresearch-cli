//! Slurm backend — submit an experiment as a batch job on a Slurm cluster.
//!
//! orx talks to the cluster's **login node** over ssh (reusing the ssh
//! backend's multiplexed transport) and drives the Slurm CLI there:
//! `sbatch --parsable` to submit, `squeue`/`sacct` to poll, `scancel` to
//! cancel. No `slurmrestd`: the REST daemon needs cluster-admin setup that
//! most users don't have, while everyone with a cluster account can ssh to
//! the login node — the same trade SkyPilot makes.
//!
//! The remote layout extends the ssh backend's convention. A run owns
//! `~/.orx/runs/<run_id>/` on the cluster's shared filesystem:
//!   repo/       the experiment branch, cloned on the login node at submit
//!               time (compute nodes often have no internet, so cloning
//!               inside the job — the other backends' pattern — would break)
//!   job.sbatch  the generated batch script (env exports + payload)
//!   log         merged stdout/stderr, captured by Slurm via `--output`
//!   exit_code   written by job.sbatch's closing lines when the payload ends
//!
//! The reattach handle is (host, slurm job id); the run dir derives from the
//! run id. Job state is read exit_code-first (scheduler-independent truth,
//! like the ssh backend), then `squeue` for live jobs, then `sacct` for jobs
//! that left the queue without writing an exit code (scancel/timeout/node
//! failure). `sacct` may be disabled cluster-wide, so it's a best-effort
//! fallback, not a dependency.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::ssh::{sh_quote, ssh_run};
use crate::error::{anyhow, Result};

// --- settings ---------------------------------------------------------------

/// User-tunable cluster defaults, stored at
/// `$XDG_CONFIG_HOME/openresearch/slurm.json`. No secrets in here — ssh
/// holds all auth. Every field is optional: a bare `sbatch` on a
/// single-partition cluster works with no configuration at all.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlurmSettings {
    /// ssh config host alias of the login node; the `--host` default.
    #[serde(default)]
    pub host: Option<String>,
    /// `#SBATCH --partition=…` default.
    #[serde(default)]
    pub partition: Option<String>,
    /// `#SBATCH --account=…` default (billing/allocation account).
    #[serde(default)]
    pub account: Option<String>,
    /// `#SBATCH --time=…` default, in orx's duration syntax ("4h", "30m").
    #[serde(default)]
    pub time_limit: Option<String>,
}

fn settings_path() -> std::path::PathBuf {
    crate::config::config_dir().join("slurm.json")
}

/// `Ok(None)` when the file is missing — slurm not configured (still usable
/// with explicit `--host`).
pub fn load_settings() -> Result<Option<SlurmSettings>> {
    let raw = match std::fs::read_to_string(settings_path()) {
        Ok(raw) => raw,
        Err(_) => return Ok(None),
    };
    match serde_json::from_str::<SlurmSettings>(&raw) {
        Ok(s) => Ok(Some(s)),
        Err(e) => Err(anyhow!(
            "Unreadable {} ({}). Fix or delete it and reconfigure.",
            settings_path().display(),
            e
        )),
    }
}

pub fn save_settings(settings: &SlurmSettings) -> Result<()> {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = format!("{}\n", serde_json::to_string_pretty(settings)?);
    std::fs::write(&path, body)?;
    Ok(())
}

// --- job spec & script generation --------------------------------------------

pub struct SlurmJobSpec {
    /// ssh config host alias of the cluster's login node.
    pub host: String,
    /// Names the remote run dir `~/.orx/runs/<run_id>`.
    pub run_id: String,
    /// Runs on the login node in the run dir at submit time, with `env`
    /// exported — the clone step. Piped to `bash -s`, never written to disk.
    pub setup_script: String,
    /// The experiment's run command; the body of the batch job (runs in
    /// `repo/` on the compute node).
    pub command: String,
    /// Exported in both the setup script and job.sbatch (tokens, synced env).
    pub env: HashMap<String, String>,
    /// `#SBATCH --gres=…`, from `--flavor` (see `resolve_gres`).
    pub gres: Option<String>,
    pub partition: Option<String>,
    pub account: Option<String>,
    /// `#SBATCH --time=…` in seconds, from `--timeout` or settings.
    pub time_limit_secs: Option<u64>,
}

/// Map a `--flavor` string onto a `--gres` request. A flavor names GPUs —
/// CPU-only runs just omit it (unlike HF/Modal there is no machine shape to
/// pick; partition + cluster defaults decide CPUs/memory).
///   "gpu"      -> gpu:1
///   "gpu:…"    -> passed through verbatim (already a GRES spec)
///   "h100:2"   -> gpu:h100:2
///   "h100"     -> gpu:h100
pub fn resolve_gres(flavor: &str) -> Option<String> {
    let f = flavor.trim();
    if f.is_empty() {
        return None;
    }
    if f == "gpu" {
        return Some("gpu:1".to_string());
    }
    if f.starts_with("gpu:") {
        return Some(f.to_string());
    }
    Some(format!("gpu:{f}"))
}

/// Seconds → Slurm's `--time` syntax (`D-HH:MM:SS` / `HH:MM:SS`).
fn slurm_time(secs: u64) -> String {
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    if days > 0 {
        format!("{days}-{h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}")
    }
}

/// Render job.sbatch. Slurm captures the whole script's stdout/stderr to
/// `log` (relative paths resolve against the submit-time cwd, the run dir,
/// which sbatch also makes the job's working directory). The payload runs in
/// a SUBSHELL so an `exit`/`set -e` inside it still reaches the
/// `echo $? > exit_code` line — same contract as the ssh backend's run.sh.
fn render_sbatch(spec: &SlurmJobSpec) -> String {
    let mut directives = vec![
        format!(
            "#SBATCH --job-name=orx-{}",
            &spec.run_id[..spec.run_id.len().min(8)]
        ),
        "#SBATCH --output=log".to_string(),
        "#SBATCH --error=log".to_string(),
        "#SBATCH --open-mode=append".to_string(),
    ];
    if let Some(secs) = spec.time_limit_secs {
        directives.push(format!("#SBATCH --time={}", slurm_time(secs)));
    }
    if let Some(p) = spec.partition.as_deref().filter(|p| !p.trim().is_empty()) {
        directives.push(format!("#SBATCH --partition={p}"));
    }
    if let Some(a) = spec.account.as_deref().filter(|a| !a.trim().is_empty()) {
        directives.push(format!("#SBATCH --account={a}"));
    }
    if let Some(g) = spec.gres.as_deref() {
        directives.push(format!("#SBATCH --gres={g}"));
    }
    // The script exits with the payload's code so Slurm's own COMPLETED/FAILED
    // verdict mirrors the payload — inspect leans on that when the exit_code
    // file is NFS-lagged behind the compute node's write.
    format!(
        "#!/usr/bin/env bash\n{directives}\n{exports}\n(\ncd repo || exit 97\n{command}\n)\ncode=$?\necho \"$code\" > exit_code\nexit \"$code\"\n",
        directives = directives.join("\n"),
        exports = render_exports(&spec.env),
        command = spec.command,
    )
}

fn render_exports(env: &HashMap<String, String>) -> String {
    let mut pairs: Vec<_> = env.iter().collect();
    pairs.sort(); // deterministic script for tests & debugging
    pairs
        .iter()
        .map(|(k, v)| format!("export {}={}", k, sh_quote(v)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `sbatch --parsable` prints `<jobid>` or `<jobid>;<cluster>`.
fn parse_job_id(out: &str) -> Result<String> {
    let id = out.trim().split(';').next().unwrap_or("").trim();
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
        return Err(anyhow!("Unexpected sbatch output: {:?}", out.trim()));
    }
    Ok(id.to_string())
}

// --- lifecycle ----------------------------------------------------------------

/// The remote run dir for a run, relative to `$HOME` (shared convention with
/// the ssh backend; derived, not stored in the descriptor).
pub fn run_dir(run_id: &str) -> String {
    format!(".orx/runs/{run_id}")
}

/// Submit the job: create the run dir, clone on the login node, write
/// job.sbatch, `sbatch --parsable`. Returns the Slurm job id — the reattach
/// handle (together with the host).
pub async fn run_job(spec: &SlurmJobSpec) -> Result<String> {
    let dir = run_dir(&spec.run_id);

    // Login-node setup (clone). Env + script travel via stdin so tokens never
    // land on an argv or in a file.
    let setup = format!(
        "{exports}\ncd \"$HOME/{dir}\" || exit 97\n{script}\n",
        exports = render_exports(&spec.env),
        script = spec.setup_script,
    );
    ssh_run(
        &spec.host,
        &format!("mkdir -p \"$HOME/{dir}\" && chmod 700 \"$HOME/{dir}\" && bash -s"),
        Some(&setup),
    )
    .await
    .map_err(|e| anyhow!("Setup on the login node failed: {e}"))?;

    // Write the batch script (owner-only: it embeds tokens) and submit.
    ssh_run(
        &spec.host,
        &format!("umask 077 && cat > \"$HOME/{dir}/job.sbatch\""),
        Some(&render_sbatch(spec)),
    )
    .await?;
    let out = ssh_run(
        &spec.host,
        &format!("cd \"$HOME/{dir}\" && sbatch --parsable job.sbatch"),
        None,
    )
    .await
    .map_err(|e| anyhow!("sbatch failed: {e}"))?;
    parse_job_id(&out)
}

/// Job state in the shared stage vocabulary (see `jobs::stage_to_run_status`).
#[derive(Debug, Clone)]
pub struct JobState {
    pub stage: String,
    pub message: Option<String>,
}

/// One combined remote probe emitting a single token — exit_code first
/// (ground truth), then live queue state, then accounting for jobs that left
/// the queue without one. POSIX-sh only: the command runs under the remote
/// user's login shell. `sacct -P` (parsable) because the default fixed-width
/// State column truncates ("CANCELLED by 1234" prints as "CANCELLED+").
///
/// `GONE` is NOT terminal by itself: it also fires when slurmctld is briefly
/// down or the exit_code write is NFS-lagged — the supervisor debounces it
/// over several polls before declaring the job lost.
pub async fn inspect_job(host: &str, run_id: &str, job_id: &str) -> Result<JobState> {
    let dir = run_dir(run_id);
    let cmd = format!(
        "d=\"$HOME/{dir}\"; \
         if [ -f \"$d/exit_code\" ]; then echo \"EXIT $(cat \"$d/exit_code\")\"; \
         else st=$(squeue -h -j {job_id} -o %T 2>/dev/null | head -n1); \
           if [ -n \"$st\" ]; then echo \"SQ $st\"; \
           else st=$(sacct -nPX -j {job_id} -o State 2>/dev/null | head -n1); \
             if [ -n \"$st\" ]; then echo \"SA $st\"; else echo GONE; fi; \
           fi; \
         fi",
    );
    let out = ssh_run(host, &cmd, None).await?;
    Ok(map_inspect_token(out.trim()))
}

/// Pure token → stage mapping (unit-tested; Slurm state names are stable).
/// Emits the internal `GONE` stage for a job the scheduler no longer knows —
/// the caller debounces it (see `inspect_job`).
fn map_inspect_token(out: &str) -> JobState {
    let state = |stage: &str, message: Option<String>| JobState {
        stage: stage.to_string(),
        message,
    };
    if let Some(code) = out.strip_prefix("EXIT ") {
        // An empty file is the window between open(O_TRUNC) and the write —
        // not a verdict; poll again.
        if code.trim().is_empty() {
            return state("RUNNING", None);
        }
        let code: i32 = code.trim().parse().unwrap_or(-1);
        return if code == 0 {
            state("COMPLETED", None)
        } else {
            state("ERROR", Some(format!("exited with code {code}")))
        };
    }
    // `squeue %T` / `sacct -P State` values. sacct suffixes cancellations
    // ("CANCELLED by 1234"), so match on the first word; the trailing-`+`
    // trim is belt-and-braces against fixed-width truncation.
    let (source, raw) = match (out.strip_prefix("SQ "), out.strip_prefix("SA ")) {
        (Some(s), _) => ("squeue", s),
        (_, Some(s)) => ("sacct", s),
        _ if out == "GONE" => return state("GONE", None),
        _ => return state("RUNNING", Some(format!("unexpected inspect output: {out}"))),
    };
    let word = raw
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_end_matches('+');
    match word {
        "PENDING" | "CONFIGURING" | "REQUEUED" | "RESV_DEL_HOLD" => state("SCHEDULING", None),
        "RUNNING" | "COMPLETING" | "SUSPENDED" | "STAGE_OUT" | "SIGNALING" => {
            state("RUNNING", None)
        }
        // Terminal per the scheduler but no exit_code file yet (NFS lag):
        // job.sbatch exits with the payload's code, so sacct's verdict mirrors
        // the payload — trust it. From squeue it's transient; keep polling.
        "COMPLETED" if source == "sacct" => state("COMPLETED", None),
        "COMPLETED" => state("RUNNING", None),
        // Also transient from squeue: default JobRequeue=1 (and
        // PreemptMode=REQUEUE) re-queues these under the same job id — a poll
        // landing in the window must not finalize a run that's about to
        // re-run. If the job is NOT requeued it leaves the queue and the
        // sacct/GONE path delivers the terminal verdict.
        "NODE_FAIL" | "PREEMPTED" if source == "squeue" => state("RUNNING", None),
        "CANCELLED" | "REVOKED" => state("CANCELED", None),
        "TIMEOUT" | "DEADLINE" => state("ERROR", Some("job hit its time limit".into())),
        "FAILED" | "NODE_FAIL" | "BOOT_FAIL" | "OUT_OF_MEMORY" | "PREEMPTED" => state(
            "ERROR",
            Some(format!("slurm reported {}", word.to_ascii_lowercase())),
        ),
        other => state(
            "RUNNING",
            Some(format!("unrecognized slurm state: {other}")),
        ),
    }
}

/// Cancel = `scancel`. Tolerant of already-finished jobs (scancel exits
/// non-zero for them); the supervisor's next poll observes the outcome.
pub async fn cancel_job(host: &str, job_id: &str) -> Result<()> {
    ssh_run(host, &format!("scancel {job_id} 2>/dev/null || true"), None).await?;
    Ok(())
}

// Log streaming reuses `ssh::stream_logs` directly — the run-dir/`log` layout
// is identical, and Slurm appends to the same file via `--output`.

// --- preflight ----------------------------------------------------------------

/// Per-host readiness for the Settings UI: reachable, Slurm CLI + git
/// present, and which partitions exist.
pub struct SlurmPreflight {
    pub reachable: bool,
    pub slurm_found: bool,
    pub git_found: bool,
    /// From `sinfo` (default partition's trailing `*` stripped).
    pub partitions: Vec<String>,
    pub error: Option<String>,
}

pub async fn preflight(host: &str) -> SlurmPreflight {
    let cmd = "if command -v sbatch >/dev/null 2>&1 && command -v squeue >/dev/null 2>&1 \
               && command -v scancel >/dev/null 2>&1; then echo SLURM_OK; fi; \
               if command -v git >/dev/null 2>&1; then echo GIT_OK; fi; \
               sinfo -h -o %P 2>/dev/null || true";
    match ssh_run(host, cmd, None).await {
        Ok(out) => {
            let mut slurm_found = false;
            let mut git_found = false;
            let mut partitions = Vec::new();
            for line in out.lines().map(str::trim).filter(|l| !l.is_empty()) {
                match line {
                    "SLURM_OK" => slurm_found = true,
                    "GIT_OK" => git_found = true,
                    p => {
                        let p = p.trim_end_matches('*').to_string();
                        if !p.is_empty() && !partitions.contains(&p) {
                            partitions.push(p);
                        }
                    }
                }
            }
            SlurmPreflight {
                reachable: true,
                slurm_found,
                git_found,
                partitions,
                error: None,
            }
        }
        Err(e) => SlurmPreflight {
            reachable: false,
            slurm_found: false,
            git_found: false,
            partitions: Vec::new(),
            error: Some(e.to_string()),
        },
    }
}

// --- tests ----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> SlurmJobSpec {
        SlurmJobSpec {
            host: "cluster".into(),
            run_id: "0123456789abcdef".into(),
            setup_script: "git clone x repo".into(),
            command: "python train.py".into(),
            env: HashMap::new(),
            gres: None,
            partition: None,
            account: None,
            time_limit_secs: None,
        }
    }

    #[test]
    fn sbatch_minimal_has_only_fixed_directives() {
        let script = render_sbatch(&spec());
        assert!(script.starts_with("#!/usr/bin/env bash\n"));
        assert!(script.contains("#SBATCH --job-name=orx-01234567\n"));
        assert!(script.contains("#SBATCH --output=log\n"));
        assert!(!script.contains("--partition"));
        assert!(!script.contains("--account"));
        assert!(!script.contains("--gres"));
        assert!(!script.contains("--time"));
        // Payload in a subshell; its code is recorded AND becomes the script's
        // exit status (Slurm's COMPLETED/FAILED must mirror the payload).
        assert!(script.ends_with(
            "(\ncd repo || exit 97\npython train.py\n)\ncode=$?\necho \"$code\" > exit_code\nexit \"$code\"\n"
        ));
    }

    #[test]
    fn sbatch_emits_optional_directives_and_quoted_env() {
        let mut s = spec();
        s.gres = Some("gpu:h100:2".into());
        s.partition = Some("gpu".into());
        s.account = Some("lab-a".into());
        s.time_limit_secs = Some(4 * 3600);
        s.env.insert("TOKEN".into(), "it's; rm -rf /".into());
        let script = render_sbatch(&s);
        assert!(script.contains("#SBATCH --gres=gpu:h100:2\n"));
        assert!(script.contains("#SBATCH --partition=gpu\n"));
        assert!(script.contains("#SBATCH --account=lab-a\n"));
        assert!(script.contains("#SBATCH --time=04:00:00\n"));
        assert!(script.contains("export TOKEN='it'\\''s; rm -rf /'\n"));
    }

    #[test]
    fn slurm_time_formats() {
        assert_eq!(slurm_time(90), "00:01:30");
        assert_eq!(slurm_time(4 * 3600), "04:00:00");
        assert_eq!(slurm_time(86_400 + 3661), "1-01:01:01");
    }

    #[test]
    fn gres_from_flavor() {
        assert_eq!(resolve_gres(""), None);
        assert_eq!(resolve_gres("gpu").as_deref(), Some("gpu:1"));
        assert_eq!(resolve_gres("gpu:a100:4").as_deref(), Some("gpu:a100:4"));
        assert_eq!(resolve_gres("h100:2").as_deref(), Some("gpu:h100:2"));
        assert_eq!(resolve_gres("h100").as_deref(), Some("gpu:h100"));
    }

    /// Live E2E against a real cluster — opt-in, never runs in CI:
    ///   ORX_SLURM_TEST_HOST=<ssh alias> cargo test jobs::slurm -- --ignored
    /// Covers submit → SCHEDULING/RUNNING → COMPLETED with logs, plus
    /// scancel → the job leaving the queue (CANCELED, or GONE where
    /// accounting is disabled — the supervisor debounces GONE).
    #[tokio::test]
    #[ignore = "needs a live slurm cluster; set ORX_SLURM_TEST_HOST"]
    async fn e2e_lifecycle_against_live_cluster() {
        let Ok(host) = std::env::var("ORX_SLURM_TEST_HOST") else {
            panic!("set ORX_SLURM_TEST_HOST to an ~/.ssh/config alias of a slurm login node");
        };
        let mk = |run_id: &str, command: &str| SlurmJobSpec {
            host: host.clone(),
            run_id: run_id.into(),
            setup_script: "mkdir -p repo".into(),
            command: command.into(),
            env: HashMap::from([("ORX_E2E".into(), "1".into())]),
            gres: None,
            partition: None,
            account: None,
            time_limit_secs: Some(300),
        };
        // 150 × 2s: a busy cluster can hold a job PENDING for a few minutes.
        let poll = |run_id: String, job_id: String, until: &'static [&'static str]| {
            let host = host.clone();
            async move {
                for _ in 0..150 {
                    let s = inspect_job(&host, &run_id, &job_id).await.unwrap();
                    if until.contains(&s.stage.as_str()) {
                        return s;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
                panic!("job {job_id} never reached {until:?}");
            }
        };

        // Fresh ids per invocation: a recycled run dir would satisfy
        // inspect_job from its stale exit_code before the job even runs.
        let run_a = format!("e2e-a-{}", uuid::Uuid::new_v4());
        let run_b = format!("e2e-b-{}", uuid::Uuid::new_v4());
        let cleanup = format!(
            "rm -rf \"$HOME/{}\" \"$HOME/{}\"",
            run_dir(&run_a),
            run_dir(&run_b)
        );

        // Happy path: runs, completes, logs arrive.
        let job_a = run_job(&mk(&run_a, "echo hello-from-slurm; echo \"env=$ORX_E2E\""))
            .await
            .unwrap();
        let done = poll(run_a.clone(), job_a, &["COMPLETED", "ERROR"]).await;
        assert_eq!(done.stage, "COMPLETED", "message: {:?}", done.message);
        let mut lines = Vec::new();
        crate::jobs::ssh::stream_logs(
            &host,
            &run_dir(&run_a),
            0,
            std::time::Duration::from_secs(5),
            &mut |l: &str| lines.push(l.to_string()),
        )
        .await
        .unwrap();
        assert!(lines.iter().any(|l| l == "hello-from-slurm"), "{lines:?}");
        assert!(lines.iter().any(|l| l == "env=1"), "{lines:?}");

        // Cancel path: scancel'd job leaves the queue without an exit code.
        let job_b = run_job(&mk(&run_b, "sleep 300")).await.unwrap();
        poll(run_b.clone(), job_b.clone(), &["RUNNING", "SCHEDULING"]).await;
        cancel_job(&host, &job_b).await.unwrap();
        let after = poll(run_b, job_b, &["CANCELED", "GONE", "ERROR"]).await;
        // Best-effort teardown before asserting — don't litter the cluster.
        let _ = ssh_run(&host, &cleanup, None).await;
        assert!(
            after.stage == "CANCELED" || after.stage == "GONE",
            "unexpected post-cancel stage: {after:?}"
        );
    }

    #[test]
    fn job_id_parsing() {
        assert_eq!(parse_job_id("123\n").unwrap(), "123");
        assert_eq!(parse_job_id("123;cluster2\n").unwrap(), "123");
        assert!(parse_job_id("sbatch: error").is_err());
        assert!(parse_job_id("").is_err());
    }

    #[test]
    fn inspect_token_mapping() {
        assert_eq!(map_inspect_token("EXIT 0").stage, "COMPLETED");
        let failed = map_inspect_token("EXIT 137");
        assert_eq!(failed.stage, "ERROR");
        assert!(failed.message.unwrap().contains("137"));
        // Empty exit_code = caught mid-write; not a verdict.
        assert_eq!(map_inspect_token("EXIT ").stage, "RUNNING");
        assert_eq!(map_inspect_token("SQ PENDING").stage, "SCHEDULING");
        assert_eq!(map_inspect_token("SQ RUNNING").stage, "RUNNING");
        assert_eq!(map_inspect_token("SQ COMPLETING").stage, "RUNNING");
        // squeue COMPLETED without exit_code: transient, keep polling.
        assert_eq!(map_inspect_token("SQ COMPLETED").stage, "RUNNING");
        // sacct COMPLETED means the script (= payload) exited 0.
        assert_eq!(map_inspect_token("SA COMPLETED").stage, "COMPLETED");
        assert_eq!(map_inspect_token("SA CANCELLED by 1234").stage, "CANCELED");
        // Fixed-width sacct truncates with a trailing '+'; -P avoids it, but
        // the parser must survive it anyway.
        assert_eq!(map_inspect_token("SA CANCELLED+").stage, "CANCELED");
        assert_eq!(map_inspect_token("SA TIMEOUT").stage, "ERROR");
        assert_eq!(map_inspect_token("SA NODE_FAIL").stage, "ERROR");
        // From squeue these are requeue-transient (JobRequeue=1); terminal
        // only once sacct (or GONE) confirms the job really left.
        assert_eq!(map_inspect_token("SQ NODE_FAIL").stage, "RUNNING");
        assert_eq!(map_inspect_token("SQ PREEMPTED").stage, "RUNNING");
        // GONE is not terminal here — the supervisor debounces it.
        assert_eq!(map_inspect_token("GONE").stage, "GONE");
        // Unknown states never wedge the supervisor into a terminal state.
        assert_eq!(map_inspect_token("SQ SOMETHING_NEW").stage, "RUNNING");
    }
}
