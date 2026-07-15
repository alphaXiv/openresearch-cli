//! SSH backend — run an experiment as a detached process on your own box.
//!
//! No scheduler: the target is a plain server you can `ssh` into. Everything
//! shells out to the `ssh` binary (like the k8s backend shells out to
//! `kubectl`), so auth is your `~/.ssh/config` + agent/keys — orx never reads a
//! key. Connections are multiplexed (ControlMaster) so the many status/log
//! polls reuse one TCP session instead of a handshake apiece.
//!
//! The handle is a remote run directory `~/.orx/runs/<run_id>/` holding:
//!   run.sh      the launcher (exported env + clone-and-run payload)
//!   log         merged stdout/stderr
//!   pid         the detached process-group leader
//!   exit_code   written when the payload finishes
//! A restarted `orx supervise` reattaches purely from that directory.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use crate::error::{anyhow, Result};

/// Where ssh keeps its ControlMaster sockets. Created on first use.
fn control_dir() -> PathBuf {
    crate::config::config_dir().join("ssh-cm")
}

/// An ssh endpoint. The classic ssh backend connects by `~/.ssh/config` alias
/// (`SshTarget::alias`); backends that learn an endpoint at runtime (an
/// OpenResearch box on a provider-assigned host:port) pass an explicit
/// `user@host` plus the options no config file knows about.
#[derive(Debug, Clone)]
pub struct SshTarget {
    /// What goes after `--`: an alias, or `user@host`.
    pub dest: String,
    /// Extra ssh args before `--` (e.g. `["-p", "2222", "-o", …]`).
    pub extra_opts: Vec<String>,
}

impl SshTarget {
    /// A bare alias — `~/.ssh/config` alone decides the endpoint.
    pub fn alias(host: &str) -> Self {
        Self {
            dest: host.to_string(),
            extra_opts: Vec::new(),
        }
    }
}

/// Shared ssh options: BatchMode (never hang on a prompt) + connection
/// multiplexing so repeated polls are cheap.
fn ssh_opts(target: &SshTarget) -> Vec<String> {
    // Not ssh's %C token: the expanded path must fit in sun_path (104 bytes
    // on macOS) and `<config dir>/ssh-cm/<40-hex>.<12-char tmp suffix>`
    // overflows it for ordinary home dirs — ssh then fails outright rather
    // than skip multiplexing. A 16-hex hash keeps it short. It folds in the
    // extra opts (where %C folds in user/host/port) so `user@host -p 2222`
    // and `user@host -p 2223` never share a control socket.
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    target.dest.hash(&mut h);
    target.extra_opts.hash(&mut h);
    let cp = control_dir().join(format!("{:016x}", h.finish()));
    let mut opts = vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=10".into(),
        "-o".into(),
        "ControlMaster=auto".into(),
        "-o".into(),
        format!("ControlPath={}", cp.display()),
        "-o".into(),
        "ControlPersist=60".into(),
    ];
    opts.extend(target.extra_opts.iter().cloned());
    opts
}

/// Run a command on `target` over ssh, feeding `stdin` if given, returning stdout.
/// A non-zero exit is an error carrying stderr (the ssh/remote failure reason).
/// Shared with the slurm backend, which drives a cluster's login node the same
/// way, and the openresearch backend, which drives a provisioned box.
pub(crate) async fn ssh_run(
    target: &SshTarget,
    remote_cmd: &str,
    stdin: Option<&str>,
) -> Result<String> {
    let _ = std::fs::create_dir_all(control_dir());
    let mut cmd = Command::new("ssh");
    cmd.args(ssh_opts(target))
        .arg("--")
        .arg(&target.dest)
        .arg(remote_cmd)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow!("`ssh` not found on PATH — the SSH backend needs the OpenSSH client.")
        } else {
            anyhow!("Could not run ssh: {e}")
        }
    })?;
    if let Some(input) = stdin {
        use tokio::io::AsyncWriteExt as _;
        if let Some(mut pipe) = child.stdin.take() {
            let _ = pipe.write_all(input.as_bytes()).await;
            drop(pipe); // EOF
        }
    }
    let out = child
        .wait_with_output()
        .await
        .map_err(|e| anyhow!("ssh wait failed: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim();
        return Err(anyhow!(
            "ssh {} failed{}: {}",
            target.dest,
            out.status
                .code()
                .map(|c| format!(" (exit {c})"))
                .unwrap_or_default(),
            if err.is_empty() { "no stderr" } else { err }
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Single-quote a value for safe embedding in the remote bash script.
pub(crate) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub struct SshJobSpec {
    /// Where to run: a config alias (the ssh backend) or an explicit endpoint.
    pub target: SshTarget,
    /// Names the remote run dir `~/.orx/runs/<run_id>`.
    pub run_id: String,
    /// The shared clone-and-run payload (`bash` script body).
    pub script: String,
    /// Exported inside run.sh on the remote (tokens, synced env).
    pub env: HashMap<String, String>,
}

/// Submit the job: write run.sh, launch it detached, record its pid. Returns
/// the remote run dir (relative to `$HOME`) — the reattach handle.
pub async fn run_job(spec: &SshJobSpec) -> Result<String> {
    let dir = format!(".orx/runs/{}", spec.run_id);
    // Default the remote job's Python to unbuffered so its prints land in `log`
    // (which we tail) live instead of block-buffering behind the redirect
    // (see jobs::default_unbuffered).
    let env = super::default_unbuffered(&spec.env);
    let exports: String = env
        .iter()
        .map(|(k, v)| format!("export {}={}", k, sh_quote(v)))
        .collect::<Vec<_>>()
        .join("\n");
    // run.sh: set up env, run the payload capturing all output to `log`, then
    // record the exit status. The payload runs in a SUBSHELL `( … )` — not a
    // `{ … }` group — so an `exit`/`set -e` failure inside it ends the subshell,
    // not run.sh, and we still reach `echo $? > exit_code`.
    let run_sh = format!(
        "#!/usr/bin/env bash\n{exports}\ncd \"$HOME/{dir}\" || exit 97\n(\n{script}\n) > log 2>&1\necho $? > exit_code\n",
        script = spec.script,
    );

    // Create the dir (owner-only) and write run.sh from stdin.
    let setup = format!(
        "mkdir -p \"$HOME/{dir}\" && chmod 700 \"$HOME/{dir}\" && cat > \"$HOME/{dir}/run.sh\"",
    );
    ssh_run(&spec.target, &setup, Some(&run_sh)).await?;

    // Launch detached so it survives the ssh channel closing. Prefer `setsid`
    // (new session → pid == pgid, so cancel can TERM the whole group); fall back
    // to `nohup` where setsid is absent (e.g. a macOS host). Record the pid.
    let launch = format!(
        "cd \"$HOME/{dir}\" && \
         if command -v setsid >/dev/null 2>&1; then setsid bash run.sh </dev/null >/dev/null 2>&1 & \
         else nohup bash run.sh </dev/null >/dev/null 2>&1 & fi; \
         echo $! > pid",
    );
    ssh_run(&spec.target, &launch, None).await?;
    Ok(dir)
}

/// Job state in the shared stage vocabulary (see `jobs::stage_to_run_status`).
#[derive(Debug, Clone)]
pub struct JobState {
    pub stage: String,
    pub message: Option<String>,
}

pub async fn inspect_job(target: &SshTarget, dir: &str) -> Result<JobState> {
    // exit_code present -> finished; pid alive -> running; pid dead & no
    // exit_code -> killed/crashed; no pid yet -> just starting.
    let cmd = format!(
        "d=\"$HOME/{dir}\"; \
         if [ -f \"$d/exit_code\" ]; then echo \"EXIT $(cat \"$d/exit_code\")\"; \
         elif [ -f \"$d/pid\" ] && kill -0 \"$(cat \"$d/pid\")\" 2>/dev/null; then echo RUNNING; \
         elif [ -f \"$d/pid\" ]; then echo DEAD; else echo PENDING; fi",
    );
    let out = ssh_run(target, &cmd, None).await?;
    let out = out.trim();
    if let Some(code) = out.strip_prefix("EXIT ") {
        let code: i32 = code.trim().parse().unwrap_or(-1);
        return Ok(if code == 0 {
            JobState {
                stage: "COMPLETED".into(),
                message: None,
            }
        } else {
            JobState {
                stage: "ERROR".into(),
                message: Some(format!("exited with code {code}")),
            }
        });
    }
    Ok(match out {
        "RUNNING" | "PENDING" => JobState {
            stage: "RUNNING".into(),
            message: None,
        },
        "DEAD" => JobState {
            stage: "ERROR".into(),
            message: Some("process died without an exit code (killed?)".into()),
        },
        other => JobState {
            stage: "RUNNING".into(),
            message: Some(format!("unexpected inspect output: {other}")),
        },
    })
}

/// One poll of the remote log past `skip` lines. Unlike the streaming backends
/// this returns promptly (the supervisor loops every ~2s); `idle` is unused.
pub async fn stream_logs(
    target: &SshTarget,
    dir: &str,
    skip: u64,
    _idle: Duration,
    sink: &mut (dyn FnMut(&str) + Send),
) -> Result<u64> {
    let cmd = format!(
        "tail -n +{} \"$HOME/{}/log\" 2>/dev/null || true",
        skip + 1,
        dir
    );
    let out = ssh_run(target, &cmd, None).await?;
    let mut seen = skip;
    // A trailing newline yields a final empty element under split('\n'); use
    // lines() which ignores it, matching the "one line = one log line" contract.
    for line in out.lines() {
        seen += 1;
        sink(line);
    }
    Ok(seen)
}

/// Cancel = TERM the process group if we have one (setsid case), else the pid
/// (nohup fallback). The negative-pid form targets the whole group.
pub async fn cancel_job(target: &SshTarget, dir: &str) -> Result<()> {
    let cmd = format!(
        "p=$(cat \"$HOME/{dir}/pid\" 2>/dev/null); \
         [ -n \"$p\" ] && {{ kill -TERM -\"$p\" 2>/dev/null || kill -TERM \"$p\" 2>/dev/null; }}; true",
    );
    ssh_run(target, &cmd, None).await?;
    Ok(())
}

/// Per-host readiness for the Settings UI: can we reach it, and is `git` there?
pub struct SshPreflight {
    pub reachable: bool,
    pub git_found: bool,
    pub error: Option<String>,
}

pub async fn preflight(target: &SshTarget) -> SshPreflight {
    match ssh_run(
        target,
        "command -v git >/dev/null 2>&1 && echo GIT_OK || echo NO_GIT",
        None,
    )
    .await
    {
        Ok(out) => SshPreflight {
            reachable: true,
            git_found: out.contains("GIT_OK"),
            error: None,
        },
        Err(e) => SshPreflight {
            reachable: false,
            git_found: false,
            error: Some(e.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_target_adds_no_extra_opts() {
        let target = SshTarget::alias("mybox");
        assert_eq!(target.dest, "mybox");
        assert!(target.extra_opts.is_empty());
        // No `-p`/`-o Strict…` beyond the shared multiplexing opts.
        assert_eq!(ssh_opts(&target).len(), 10);
    }

    /// Explicit targets on the same host but different ports must not share a
    /// ControlMaster socket — the opts are part of the ControlPath hash.
    #[test]
    fn control_path_differs_per_port() {
        let control_path = |t: &SshTarget| {
            ssh_opts(t)
                .into_iter()
                .find(|o| o.starts_with("ControlPath="))
                .unwrap()
        };
        let mk = |port: &str| SshTarget {
            dest: "root@h".to_string(),
            extra_opts: vec!["-p".into(), port.into()],
        };
        assert_ne!(control_path(&mk("22022")), control_path(&mk("22023")));
        assert_eq!(control_path(&mk("22022")), control_path(&mk("22022")));
    }
}
