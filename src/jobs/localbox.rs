//! Local backend — run an experiment as a detached process on this machine.
//!
//! The no-transport twin of `jobs/ssh.rs`: same run-dir layout
//!   run.sh      the launcher (exported env + clone-and-run payload)
//!   log         merged stdout/stderr
//!   pid         the detached process-group leader
//!   exit_code   written when the payload finishes
//! but under the orx data dir (`<data dir>/local-runs/<run_id>/`) instead of a
//! remote `~/.orx/runs/`. A restarted `orx supervise` reattaches purely from
//! that directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{anyhow, Result};
use crate::jobs::ssh::{sh_quote, JobState};

/// The run's working directory: `<data dir>/local-runs/<run id>`.
pub fn run_dir(run_id: &str) -> PathBuf {
    // Run ids are locally-minted UUIDs; sanitize anyway (same as log_path).
    let safe: String = run_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    crate::store::data_dir().join("local-runs").join(safe)
}

pub struct LocalJobSpec {
    /// Names the run dir `<data dir>/local-runs/<run_id>`.
    pub run_id: String,
    /// The shared clone-and-run payload (`bash` script body).
    pub script: String,
    /// Exported inside run.sh (tokens, synced env) — written owner-only.
    pub env: HashMap<String, String>,
}

/// Submit the job: write run.sh, launch it detached in its own process group
/// (pid == pgid, so cancel can TERM the whole tree), record the pid. Returns
/// the run dir — the reattach handle stored on the descriptor.
pub fn run_job(spec: &LocalJobSpec) -> Result<PathBuf> {
    let dir = run_dir(&spec.run_id);
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow!("Could not create {}: {}", dir.display(), e))?;
    // Default the job's Python to unbuffered so its prints land in `log` (which
    // we tail) live instead of block-buffering behind the redirect (see
    // jobs::default_unbuffered).
    let env = super::default_unbuffered(&spec.env);
    let exports: String = env
        .iter()
        .map(|(k, v)| format!("export {}={}", k, sh_quote(v)))
        .collect::<Vec<_>>()
        .join("\n");
    // Same subshell shape as the ssh backend: an `exit`/`set -e` failure inside
    // `( … )` ends the subshell, not run.sh, so exit_code is always written.
    let run_sh = format!(
        "#!/usr/bin/env bash\n{exports}\ncd {dir} || exit 97\n(\n{script}\n) > log 2>&1\necho $? > exit_code\n",
        dir = sh_quote(&dir.to_string_lossy()),
        script = spec.script,
    );
    let run_sh_path = dir.join("run.sh");
    std::fs::write(&run_sh_path, run_sh)
        .map_err(|e| anyhow!("Could not write {}: {}", run_sh_path.display(), e))?;
    #[cfg(unix)]
    {
        // run.sh carries exported tokens — keep both it and the dir owner-only.
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        let _ = std::fs::set_permissions(&run_sh_path, std::fs::Permissions::from_mode(0o600));
    }

    let mut cmd = std::process::Command::new("bash");
    cmd.arg("run.sh")
        .current_dir(&dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let child = cmd
        .spawn()
        .map_err(|e| anyhow!("Could not launch the local run: {}", e))?;
    std::fs::write(dir.join("pid"), format!("{}\n", child.id()))
        .map_err(|e| anyhow!("Could not record the run's pid: {}", e))?;
    Ok(dir)
}

/// Is the recorded process still alive? `ps` rather than `kill -0`: a zombie
/// (dead but not yet reaped by a still-living spawner) answers `kill -0` yet
/// is not running. No libc dependency; works on macOS and Linux.
fn pid_alive(pid: &str) -> bool {
    match std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", pid])
        .stderr(std::process::Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => {
            let stat = String::from_utf8_lossy(&o.stdout);
            let stat = stat.trim();
            !stat.is_empty() && !stat.starts_with('Z')
        }
        _ => false,
    }
}

/// The terminal state recorded in exit_code, if any. An empty file is run.sh
/// mid-write (`>` truncates before the code lands) — not terminal yet.
fn exit_code_state(dir: &Path) -> Option<JobState> {
    let raw = std::fs::read_to_string(dir.join("exit_code")).ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let code: i32 = raw.parse().unwrap_or(-1);
    Some(if code == 0 {
        JobState {
            stage: "COMPLETED".into(),
            message: None,
        }
    } else {
        JobState {
            stage: "ERROR".into(),
            message: Some(format!("exited with code {code}")),
        }
    })
}

/// Job state in the shared stage vocabulary (see `jobs::stage_to_run_status`).
/// exit_code present -> finished; pid alive -> running; pid dead & no
/// exit_code -> killed/crashed.
pub fn inspect_job(dir: &Path) -> JobState {
    if let Some(state) = exit_code_state(dir) {
        return state;
    }
    match std::fs::read_to_string(dir.join("pid")) {
        Ok(pid) if pid_alive(pid.trim()) => JobState {
            stage: "RUNNING".into(),
            message: None,
        },
        // Dead pid: run.sh may have written exit_code and exited between the
        // check above and the ps probe — re-read before calling it killed.
        Ok(_) => exit_code_state(dir).unwrap_or(JobState {
            stage: "ERROR".into(),
            message: Some("process died without an exit code (killed?)".into()),
        }),
        // pid not written yet — just starting.
        Err(_) => JobState {
            stage: "RUNNING".into(),
            message: None,
        },
    }
}

/// One poll of the log past `skip` lines (the supervisor loops every ~2s).
/// A missing log file just means the payload hasn't printed yet.
pub fn stream_logs(dir: &Path, skip: u64, sink: &mut (dyn FnMut(&str) + Send)) -> Result<u64> {
    let content = match std::fs::read_to_string(dir.join("log")) {
        Ok(c) => c,
        Err(_) => return Ok(skip),
    };
    let mut seen = skip;
    for line in content.lines().skip(skip as usize) {
        seen += 1;
        sink(line);
    }
    Ok(seen)
}

/// Cancel = TERM the process group (pid == pgid under `process_group(0)`);
/// fall back to the pid alone if the group kill is refused.
pub fn cancel_job(dir: &Path) -> Result<()> {
    let pid = std::fs::read_to_string(dir.join("pid"))
        .map_err(|e| anyhow!("Could not read the run's pid: {}", e))?;
    let pid = pid.trim().to_string();
    let group = std::process::Command::new("kill")
        .args(["-TERM", "--", &format!("-{pid}")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !group {
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    Ok(())
}

/// What "this machine" is, for the Compute settings card: the hardware a
/// `--backend local` run gets. Matters most when the dashboard is reached over
/// port forwarding from a GPU box — the card is how the user sees they're
/// sitting on real GPUs.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalHardware {
    pub hostname: String,
    pub os: &'static str,
    pub arch: &'static str,
    /// CPU brand string on macOS (e.g. "Apple M2 Pro"); NVIDIA-less Linux
    /// boxes just show cores/RAM.
    pub chip: Option<String>,
    pub cpu_count: usize,
    pub mem_bytes: Option<u64>,
    pub gpus: Vec<Gpu>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Gpu {
    pub name: String,
    pub mem_mib: Option<u64>,
}

/// Best-effort hardware probe. Every field degrades independently — a missing
/// `nvidia-smi` (or any probe failure) is an empty GPU list, never an error.
/// Blocking (subprocesses); call via `spawn_blocking` from async handlers.
pub fn hardware_info() -> LocalHardware {
    let cmd = |name: &str, args: &[&str]| -> Option<String> {
        let out = std::process::Command::new(name).args(args).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let mem_bytes = if cfg!(target_os = "macos") {
        cmd("sysctl", &["-n", "hw.memsize"]).and_then(|s| s.parse().ok())
    } else {
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|raw| {
                // "MemTotal:       32763528 kB"
                raw.lines()
                    .find(|l| l.starts_with("MemTotal:"))?
                    .split_whitespace()
                    .nth(1)?
                    .parse::<u64>()
                    .ok()
                    .map(|kb| kb * 1024)
            })
    };
    LocalHardware {
        hostname: cmd("hostname", &[]).unwrap_or_else(|| "unknown".to_string()),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        chip: if cfg!(target_os = "macos") {
            cmd("sysctl", &["-n", "machdep.cpu.brand_string"])
        } else {
            None
        },
        cpu_count: std::thread::available_parallelism().map_or(0, |n| n.get()),
        mem_bytes,
        gpus: cmd(
            "nvidia-smi",
            &[
                "--query-gpu=name,memory.total",
                "--format=csv,noheader,nounits",
            ],
        )
        .map(|out| parse_nvidia_smi_csv(&out))
        .unwrap_or_default(),
    }
}

/// Parse `nvidia-smi --query-gpu=name,memory.total --format=csv,noheader,nounits`:
/// one `name, mem_mib` line per GPU. Names may contain no commas in this
/// format (nvidia-smi separates fields with ", "), but tolerate odd lines by
/// keeping the name and dropping the memory rather than dropping the GPU.
fn parse_nvidia_smi_csv(out: &str) -> Vec<Gpu> {
    out.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| match line.rsplit_once(',') {
            Some((name, mem)) => Gpu {
                name: name.trim().to_string(),
                mem_mib: mem.trim().parse().ok(),
            },
            None => Gpu {
                name: line.trim().to_string(),
                mem_mib: None,
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvidia_smi_csv_two_gpus() {
        let parsed =
            parse_nvidia_smi_csv("NVIDIA A100-SXM4-80GB, 81920\nNVIDIA A100-SXM4-80GB, 81920\n");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name, "NVIDIA A100-SXM4-80GB");
        assert_eq!(parsed[0].mem_mib, Some(81920));
    }

    #[test]
    fn nvidia_smi_csv_empty_and_malformed() {
        assert!(parse_nvidia_smi_csv("").is_empty());
        assert!(parse_nvidia_smi_csv("\n  \n").is_empty());
        let odd = parse_nvidia_smi_csv("Tesla T4");
        assert_eq!(odd.len(), 1, "GPU kept even without a memory field");
        assert_eq!(odd[0].name, "Tesla T4");
        assert_eq!(odd[0].mem_mib, None);
    }

    fn wait_terminal(dir: &Path) -> JobState {
        let mut state = inspect_job(dir);
        for _ in 0..100 {
            if state.stage != "RUNNING" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
            state = inspect_job(dir);
        }
        state
    }

    #[test]
    fn local_job_lifecycle() {
        // The only test that touches ORX_DATA_DIR, so the global env is safe.
        let base = std::env::temp_dir().join(format!("orx-localbox-test-{}", std::process::id()));
        std::env::set_var("ORX_DATA_DIR", &base);

        let dir = run_job(&LocalJobSpec {
            run_id: "lifecycle".into(),
            script: "echo hello-$ORX_TEST_VAR".into(),
            env: HashMap::from([("ORX_TEST_VAR".to_string(), "42".to_string())]),
        })
        .unwrap();
        let state = wait_terminal(&dir);
        assert_eq!(state.stage, "COMPLETED", "message: {:?}", state.message);
        // Python is defaulted to unbuffered so tailed-`log` output streams live.
        let run_sh = std::fs::read_to_string(dir.join("run.sh")).unwrap();
        assert!(run_sh.contains("export PYTHONUNBUFFERED='1'\n"));

        let mut lines = Vec::new();
        let seen = stream_logs(&dir, 0, &mut |l| lines.push(l.to_string())).unwrap();
        assert_eq!(seen, 1);
        assert_eq!(lines, ["hello-42"]);
        // Re-poll past the consumed lines: nothing new.
        assert_eq!(stream_logs(&dir, seen, &mut |_| ()).unwrap(), seen);

        let failed = run_job(&LocalJobSpec {
            run_id: "failing".into(),
            script: "exit 3".into(),
            env: HashMap::new(),
        })
        .unwrap();
        let state = wait_terminal(&failed);
        assert_eq!(state.stage, "ERROR");
        assert_eq!(state.message.as_deref(), Some("exited with code 3"));

        let cancelled = run_job(&LocalJobSpec {
            run_id: "cancelled".into(),
            script: "sleep 60".into(),
            env: HashMap::new(),
        })
        .unwrap();
        assert_eq!(inspect_job(&cancelled).stage, "RUNNING");
        cancel_job(&cancelled).unwrap();
        let state = wait_terminal(&cancelled);
        // TERM leaves either a dead pid with no exit_code, or a non-zero
        // exit_code if run.sh got to write one — ERROR either way.
        assert_eq!(state.stage, "ERROR");

        std::env::remove_var("ORX_DATA_DIR");
        let _ = std::fs::remove_dir_all(&base);
    }
}
