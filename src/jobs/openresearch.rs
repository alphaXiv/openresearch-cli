//! OpenResearch backend — an ephemeral platform box per run.
//!
//! Unlike the other backends, the compute itself comes from the OpenResearch
//! API: submit provisions an org-billed GPU/CPU sandbox (`POST /sandboxes`),
//! the supervisor polls `GET /sandboxes/{id}` until the box is online, runs
//! the clone-and-run payload on it over ssh (the ssh backend's transport and
//! run-dir layout, via `BackendDescriptor::openresearch_ssh_target`), and
//! deletes the box (`DELETE /sandboxes/{id}`) once the run is terminal.
//! Auth is the `orx login` credentials, not a backend-specific token.

use std::time::Duration;

use crate::client::{delete_sandbox, get_sandbox, Sandbox, SandboxTarget};
use crate::config::Credentials;
use crate::error::{anyhow, Result};

/// How long provisioning may take before the run is failed and the box
/// deleted. GPU boxes usually come online in single-digit minutes; a box
/// stuck longer is billing for nothing.
pub const PROVISION_DEADLINE: Duration = Duration::from_secs(15 * 60);

const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// The remote run dir for a run, relative to `$HOME` (shared convention with
/// the ssh backend; derived, not stored in the descriptor).
pub fn run_dir(run_id: &str) -> String {
    format!(".orx/runs/{run_id}")
}

/// Parse `--flavor` into a `POST /sandboxes` target: `<gpu_id>[:count]`
/// (e.g. `h100_sxm:2`) or a CPU flavor `cpu…[:vcpus]` (e.g. `cpu5c:8`).
/// Ids are validated server-side against the live catalog (400 on unknown),
/// same as the managed `--gpu` path — see `orx compute` for what exists.
pub fn parse_flavor(flavor: &str, disk_gb: i64, provider: Option<String>) -> Result<SandboxTarget> {
    let flavor = flavor.trim();
    let (base, count) = match flavor.split_once(':') {
        Some((base, count)) => {
            let count: i64 = count.parse().ok().filter(|c| *c >= 1).ok_or_else(|| {
                anyhow!(
                    "Bad --flavor '{flavor}': the ':{count}' suffix must be a positive \
                     count (GPUs) or vCPU tier. See `orx compute` for available shapes."
                )
            })?;
            (base, Some(count))
        }
        None => (flavor, None),
    };
    if base.is_empty() {
        return Err(anyhow!(
            "--flavor is empty. Pass a GPU id like h100_sxm[:count] or a CPU flavor \
             like cpu5c[:vcpus] — see `orx compute`."
        ));
    }
    if base.starts_with("cpu") {
        Ok(SandboxTarget::NewCpu {
            cpu_flavor: base.to_string(),
            vcpu_count: count.unwrap_or(8),
        })
    } else {
        Ok(SandboxTarget::New {
            gpu: base.to_string(),
            gpu_count: count.unwrap_or(1),
            disk_gb,
            provider,
        })
    }
}

/// Wrap the clone-and-run payload in a wall-clock guard so a hung run can't
/// bill the box forever. TERM first (checkpoint-friendly), KILL 30s later.
/// GNU coreutils `timeout` is on the box image.
pub fn wrap_with_timeout(script: &str, timeout_secs: u64) -> String {
    format!(
        "timeout --signal=TERM --kill-after=30s {timeout_secs} bash -c {q}\n\
         rc=$?\n\
         if [ \"$rc\" = 124 ]; then echo \"orx: run timed out after {timeout_secs}s\" >&2; fi\n\
         exit $rc",
        q = super::ssh::sh_quote(script),
    )
}

/// What `wait_online` resolved to when it didn't error.
pub enum WaitOutcome {
    /// The box is online with its SSH endpoint populated.
    Online(Box<Sandbox>),
    /// The caller's cancel check fired first; the box may still be booting.
    Cancelled,
}

/// Poll the box until it is online (SSH endpoint known), the deadline passes,
/// or `cancel_check` fires. `offline`/`dead` mid-provision is a hard error —
/// the box will never come up. Transient API errors are retried until the
/// deadline; the caller tears the box down on every non-`Online` outcome.
pub async fn wait_online(
    creds: &Credentials,
    sandbox_id: &str,
    deadline: Duration,
    mut cancel_check: impl FnMut() -> bool,
) -> Result<WaitOutcome> {
    let started = std::time::Instant::now();
    let mut last_err: Option<String> = None;
    loop {
        if cancel_check() {
            return Ok(WaitOutcome::Cancelled);
        }
        if started.elapsed() > deadline {
            return Err(anyhow!(
                "Box {sandbox_id} did not come online within {}m{}.",
                deadline.as_secs() / 60,
                last_err
                    .map(|e| format!(" (last API error: {e})"))
                    .unwrap_or_default()
            ));
        }
        match get_sandbox(creds, sandbox_id).await {
            Ok(envelope) => {
                let sandbox = envelope.sandbox;
                match sandbox.status.as_str() {
                    "online"
                        if sandbox.ssh_hostname.is_some()
                            && sandbox.ssh_port.is_some()
                            && sandbox.ssh_username.is_some() =>
                    {
                        return Ok(WaitOutcome::Online(Box::new(sandbox)));
                    }
                    "offline" | "dead" => {
                        return Err(anyhow!(
                            "Box {sandbox_id} went {} while provisioning{}.",
                            sandbox.status,
                            sandbox
                                .provision_warnings
                                .map(|w| format!(": {w}"))
                                .unwrap_or_default()
                        ));
                    }
                    _ => {}
                }
                last_err = None;
            }
            Err(err) => last_err = Some(err.to_string()),
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Whether the run dir on the box was already launched (pid or exit_code
/// present). Distinguishes a supervisor restart mid-run (reattach, don't
/// relaunch) from one that died between recording the endpoint and launching.
pub async fn launched(target: &super::ssh::SshTarget, run_id: &str) -> Result<bool> {
    let dir = run_dir(run_id);
    let out = super::ssh::ssh_run(
        target,
        &format!(
            "d=\"$HOME/{dir}\"; \
             if [ -e \"$d/pid\" ] || [ -e \"$d/exit_code\" ]; then echo STARTED; else echo FRESH; fi"
        ),
        None,
    )
    .await?;
    Ok(out.contains("STARTED"))
}

/// Delete the box, retrying transient failures. A 404 is success — the box is
/// already gone (dashboard delete, billing sweeper) — which makes teardown
/// idempotent across supervisor restarts.
pub async fn teardown(creds: &Credentials, sandbox_id: &str) -> Result<()> {
    let mut last = None;
    for backoff_secs in [0u64, 2, 5, 10] {
        if backoff_secs > 0 {
            tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
        }
        match delete_sandbox(creds, sandbox_id).await {
            Ok(()) => return Ok(()),
            Err(err) if err.to_string().contains("(404 ") => return Ok(()),
            Err(err) => last = Some(err),
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("teardown failed")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_flavor_gpu_defaults_to_one() {
        match parse_flavor("h100_sxm", 100, None).unwrap() {
            SandboxTarget::New {
                gpu,
                gpu_count,
                disk_gb,
                provider,
            } => {
                assert_eq!(gpu, "h100_sxm");
                assert_eq!(gpu_count, 1);
                assert_eq!(disk_gb, 100);
                assert!(provider.is_none());
            }
            other => panic!("wrong target: {other:?}"),
        }
    }

    #[test]
    fn parse_flavor_gpu_with_count_and_provider() {
        match parse_flavor("h100_sxm:2", 250, Some("runpod".into())).unwrap() {
            SandboxTarget::New {
                gpu,
                gpu_count,
                disk_gb,
                provider,
            } => {
                assert_eq!(gpu, "h100_sxm");
                assert_eq!(gpu_count, 2);
                assert_eq!(disk_gb, 250);
                assert_eq!(provider.as_deref(), Some("runpod"));
            }
            other => panic!("wrong target: {other:?}"),
        }
    }

    #[test]
    fn parse_flavor_cpu_defaults_to_eight_vcpus() {
        match parse_flavor("cpu5c", 100, None).unwrap() {
            SandboxTarget::NewCpu {
                cpu_flavor,
                vcpu_count,
            } => {
                assert_eq!(cpu_flavor, "cpu5c");
                assert_eq!(vcpu_count, 8);
            }
            other => panic!("wrong target: {other:?}"),
        }
    }

    #[test]
    fn parse_flavor_cpu_with_vcpus() {
        match parse_flavor("cpu5m:32", 100, None).unwrap() {
            SandboxTarget::NewCpu {
                cpu_flavor,
                vcpu_count,
            } => {
                assert_eq!(cpu_flavor, "cpu5m");
                assert_eq!(vcpu_count, 32);
            }
            other => panic!("wrong target: {other:?}"),
        }
    }

    #[test]
    fn parse_flavor_rejects_bad_suffix_and_empty() {
        assert!(parse_flavor("h100_sxm:x", 100, None).is_err());
        assert!(parse_flavor("h100_sxm:0", 100, None).is_err());
        assert!(parse_flavor("", 100, None).is_err());
        assert!(parse_flavor(":2", 100, None).is_err());
    }

    #[test]
    fn timeout_wrapper_quotes_and_guards() {
        let wrapped = wrap_with_timeout("echo 'hi there'", 14400);
        assert!(wrapped.starts_with("timeout --signal=TERM --kill-after=30s 14400 bash -c "));
        // The payload survives quoting (embedded single quotes escaped).
        assert!(wrapped.contains("'echo '\\''hi there'\\'''"));
        assert!(wrapped.contains("rc=$?"));
        assert!(wrapped.trim_end().ends_with("exit $rc"));
    }

    #[test]
    fn run_dir_matches_ssh_convention() {
        assert_eq!(run_dir("abc"), ".orx/runs/abc");
    }
}
