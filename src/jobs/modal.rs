//! Modal Sandboxes backend — ephemeral serverless containers on Modal.com.
//!
//! Modal is Python-SDK-driven (there is no generic REST job-submit API), so
//! every operation shells out to a bundled Python launcher that drives
//! `modal.Sandbox`. Sandboxes are reconnectable by id (`Sandbox.from_id`),
//! which is exactly what lets the detached `orx supervise` reattach from a
//! fresh process — the same reattach model the HF and k8s backends rely on.
//!
//! Transport is `<python> -c <LAUNCHER> <subcommand> [args]`, mirroring how the
//! k8s backend shells out to `kubectl`. The interpreter is an orx-managed venv
//! with the `modal` SDK, auto-provisioned on first launch (`ensure_env`), so the
//! user never has to pip-install `modal` or juggle `$ORX_MODAL_PYTHON`. Auth is
//! Modal's own: MODAL_TOKEN_ID / MODAL_TOKEN_SECRET (process env or the synced
//! `~/.openresearch/env`) or `~/.modal.toml`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::process::Command;

use crate::error::{anyhow, Result};

/// Bundled Python launcher. Subcommands (argv[1]):
///   submit          spec JSON on stdin -> prints {"sandboxId": "sb-..."}
///   status <id>     -> prints {"stage": "...", "message": "..."?}
///   logs   <id>     streams the sandbox's merged stdout/stderr, line per line
///   cancel <id>     terminate the sandbox
///
/// Exit 3 = the `modal` package isn't importable (surfaced as a friendly hint).
const LAUNCHER: &str = r#"
import sys, json

def _modal():
    try:
        import modal
        return modal
    except Exception:
        sys.stderr.write("modal-not-installed")
        sys.exit(3)

def submit():
    modal = _modal()
    spec = json.load(sys.stdin)
    app = modal.App.lookup(spec.get("app", "openresearch"), create_if_missing=True)
    image = modal.Image.from_registry(spec["image"])
    kwargs = {
        "app": app,
        "image": image,
        "timeout": int(spec.get("timeoutSeconds", 3600)),
    }
    if spec.get("gpu"):
        kwargs["gpu"] = spec["gpu"]
    if spec.get("cpu"):
        kwargs["cpu"] = float(spec["cpu"])
    if spec.get("memory"):
        kwargs["memory"] = int(spec["memory"])
    env = spec.get("env") or {}
    if env:
        # Tokens ride an ephemeral Secret, never the plain `env` arg (which
        # shows up in the Modal dashboard).
        kwargs["secrets"] = [modal.Secret.from_dict(env)]
    tags = spec.get("tags") or {}
    if tags:
        kwargs["tags"] = tags
    sb = modal.Sandbox.create("bash", "-c", spec["script"], **kwargs)
    print(json.dumps({"sandboxId": sb.object_id}))

def status(sid):
    modal = _modal()
    sb = modal.Sandbox.from_id(sid)
    code = sb.poll()  # None while running, exit code once finished
    if code is None:
        out = {"stage": "RUNNING"}
    elif code == 0:
        out = {"stage": "COMPLETED"}
    else:
        out = {"stage": "ERROR", "message": "exited with code %d" % code}
    print(json.dumps(out))

def logs(sid):
    modal = _modal()
    sb = modal.Sandbox.from_id(sid)
    # StreamReader replays from the start on each (re)connect, which matches
    # supervise's skip/dedup contract (same as HF's SSE and `kubectl logs -f`).
    for line in sb.stdout:
        if not line.endswith("\n"):
            line += "\n"
        sys.stdout.write(line)
        sys.stdout.flush()

def cancel(sid):
    modal = _modal()
    modal.Sandbox.from_id(sid).terminate()

cmd = sys.argv[1] if len(sys.argv) > 1 else ""
if cmd == "submit":
    submit()
elif cmd == "status":
    status(sys.argv[2])
elif cmd == "logs":
    logs(sys.argv[2])
elif cmd == "cancel":
    cancel(sys.argv[2])
else:
    sys.stderr.write("unknown launcher subcommand: %r" % cmd)
    sys.exit(2)
"#;

/// Directory of the orx-managed Modal environment (a venv with `modal` in it),
/// under orx's config dir alongside `k8s.json`.
fn managed_env_dir() -> PathBuf {
    crate::config::config_dir().join("envs").join("modal")
}

/// The interpreter inside the managed env (may not exist until `ensure_env`).
fn managed_python() -> PathBuf {
    let dir = managed_env_dir();
    if cfg!(windows) {
        dir.join("Scripts").join("python.exe")
    } else {
        dir.join("bin").join("python")
    }
}

/// The Python interpreter the launcher runs with — resolved with NO side
/// effects, so it's safe to call from the detached supervisor:
///   1. `$ORX_MODAL_PYTHON` (explicit override)
///   2. the orx-managed venv, once `ensure_env` has provisioned it
///   3. system `python3` (bootstrap / detection fallback)
fn python_bin() -> String {
    if let Ok(p) = std::env::var("ORX_MODAL_PYTHON") {
        if !p.trim().is_empty() {
            return p;
        }
    }
    let managed = managed_python();
    if managed.exists() {
        return managed.to_string_lossy().into_owned();
    }
    "python3".to_string()
}

/// Does `py -c "import modal"` succeed?
async fn imports_modal(py: &str) -> bool {
    Command::new(py)
        .args(["-c", "import modal"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// First interpreter on PATH that can build a venv (to bootstrap the managed env).
async fn base_python() -> Option<String> {
    for c in ["python3", "python"] {
        let ok = Command::new(c)
            .args(["-c", "import venv"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return Some(c.to_string());
        }
    }
    None
}

/// Ensure a usable Modal interpreter exists, provisioning the orx-managed venv
/// on first use so `--backend modal` "just works" without the user pip-installing
/// `modal` or setting `ORX_MODAL_PYTHON`. Idempotent and fast once built; a
/// no-op when `$ORX_MODAL_PYTHON` is set (the user owns that interpreter).
/// Prints one-time progress to stderr. Only the launch path calls this — the
/// supervisor relies on the env already being there.
pub async fn ensure_env() -> Result<()> {
    if std::env::var("ORX_MODAL_PYTHON").is_ok_and(|p| !p.trim().is_empty()) {
        return Ok(()); // explicit override — trust it, don't provision
    }
    let managed = managed_python();
    let managed_str = managed.to_string_lossy().into_owned();
    if managed.exists() && imports_modal(&managed_str).await {
        return Ok(()); // already provisioned and healthy
    }
    let base = base_python().await.ok_or_else(|| {
        anyhow!(
            "No Python 3 found to build the Modal environment. Install Python 3 (it ships with \
             `venv`), or set ORX_MODAL_PYTHON to an interpreter that already has `modal`."
        )
    })?;
    let dir = managed_env_dir();
    if !managed.exists() {
        eprintln!(
            "orx: provisioning the Modal environment at {} (one-time, ~30–60s)…",
            dir.display()
        );
        if let Some(parent) = dir.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let ok = Command::new(&base)
            .arg("-m")
            .arg("venv")
            .arg(&dir)
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return Err(anyhow!(
                "Could not create a virtualenv at {} with {}.",
                dir.display(),
                base
            ));
        }
    }
    eprintln!("orx: installing the `modal` SDK…");
    let ok = Command::new(&managed_str)
        .args([
            "-m",
            "pip",
            "install",
            "--quiet",
            "--disable-pip-version-check",
            "modal",
        ])
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok || !imports_modal(&managed_str).await {
        return Err(anyhow!(
            "Built the Modal environment but `import modal` still fails. Remove {} and relaunch, \
             or set ORX_MODAL_PYTHON to a working interpreter.",
            dir.display()
        ));
    }
    eprintln!("orx: Modal environment ready.");
    Ok(())
}

/// Modal auth to inject into the launcher subprocess: the inherited process env
/// wins, otherwise the box's synced env file (`~/.openresearch/env`) — so creds
/// set in `orx up` Settings → Environment reach Modal even when orx's own
/// process env doesn't carry them. (`~/.modal.toml` is picked up by the SDK
/// directly and needs nothing here.)
fn modal_auth_env() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for key in ["MODAL_TOKEN_ID", "MODAL_TOKEN_SECRET"] {
        if std::env::var_os(key).is_some() {
            continue; // already inherited
        }
        if let Some(v) = crate::config::synced_env_var(key) {
            out.push((key.to_string(), v));
        }
    }
    out
}

fn missing_python_hint(e: &std::io::Error) -> crate::error::Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        anyhow!(
            "{} not found on PATH — Modal is driven through its Python SDK. Install Python 3 \
             and `pip install modal`, then `modal token new` (or set MODAL_TOKEN_ID / \
             MODAL_TOKEN_SECRET). Point orx at a specific interpreter with ORX_MODAL_PYTHON.",
            python_bin()
        )
    } else {
        anyhow!("Could not run {}: {}", python_bin(), e)
    }
}

/// Map a launcher non-zero exit into a useful error (exit 3 = no `modal`).
fn launcher_error(what: &str, code: Option<i32>, stderr: &str) -> crate::error::Error {
    let stderr = stderr.trim();
    if code == Some(3) || stderr == "modal-not-installed" {
        return anyhow!(
            "The `modal` Python package isn't importable by {}. Run `pip install modal` \
             (and `modal token new`), or set ORX_MODAL_PYTHON to an interpreter that has it.",
            python_bin()
        );
    }
    anyhow!(
        "modal {} failed{}: {}",
        what,
        code.map(|c| format!(" (exit {c})")).unwrap_or_default(),
        if stderr.is_empty() {
            "no stderr"
        } else {
            stderr
        }
    )
}

/// Run the launcher and capture its output (submit / status / cancel).
async fn launcher_capture(args: &[&str], stdin: Option<&str>) -> Result<Vec<u8>> {
    let mut cmd = Command::new(python_bin());
    cmd.arg("-c")
        .arg(LAUNCHER)
        .args(args)
        // Unbuffer the launcher's own stdout: we read it line-by-line, so block
        // buffering (stdout is a pipe here, not a TTY) would stall log streaming.
        .env(super::PYTHONUNBUFFERED, "1")
        .envs(modal_auth_env())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| missing_python_hint(&e))?;
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
        .map_err(|e| anyhow!("Could not run {}: {}", python_bin(), e))?;
    if !out.status.success() {
        return Err(launcher_error(
            args.first().copied().unwrap_or("?"),
            out.status.code(),
            &String::from_utf8_lossy(&out.stderr),
        ));
    }
    Ok(out.stdout)
}

/// Resources resolved from a flavor name.
pub struct ModalResources {
    /// Modal GPU spec (e.g. "A10G", "A100-80GB", "H100:2"), or `None` for CPU.
    pub gpu: Option<String>,
    pub cpu: Option<f64>,
    pub memory: Option<u64>,
}

/// Map a `--flavor` name to Modal resources. CPU flavors (name starts with
/// "cpu") get no GPU; anything else is treated as a Modal GPU spec and passed
/// through uppercased, so `--flavor a100-80gb` and `--flavor h100:2` both work.
pub fn resolve_flavor(name: &str) -> ModalResources {
    let n = name.trim();
    if n.is_empty() || n.eq_ignore_ascii_case("cpu") {
        return ModalResources {
            gpu: None,
            cpu: None,
            memory: None,
        };
    }
    if let Some(rest) = n.strip_prefix("cpu-").or_else(|| n.strip_prefix("cpu_")) {
        // cpu-large / cpu-xlarge -> a bit more CPU/RAM; otherwise defaults.
        let (cpu, memory) = match rest.to_ascii_lowercase().as_str() {
            "large" => (Some(8.0), Some(32768)),
            "xlarge" => (Some(16.0), Some(65536)),
            _ => (None, None),
        };
        return ModalResources {
            gpu: None,
            cpu,
            memory,
        };
    }
    ModalResources {
        gpu: Some(n.to_ascii_uppercase()),
        cpu: None,
        memory: None,
    }
}

/// Default docker image: plain python for CPU, a CUDA-ready pytorch image for
/// GPU — same families as the HF/k8s defaults.
pub fn default_image(gpu: bool) -> String {
    if gpu {
        "pytorch/pytorch:2.6.0-cuda12.4-cudnn9-runtime".to_string()
    } else {
        "python:3.12".to_string()
    }
}

pub struct ModalJobSpec {
    /// `bash -c` payload (the shared clone-and-run script).
    pub script: String,
    pub image: String,
    pub gpu: Option<String>,
    pub cpu: Option<f64>,
    pub memory: Option<u64>,
    /// Passed to the sandbox as an ephemeral Secret (tokens, synced env).
    pub env: HashMap<String, String>,
    pub timeout_seconds: u64,
    /// Modal app name to group sandboxes under.
    pub app: String,
    /// Sandbox tags (or_run / or_experiment / or_project) for observability.
    pub tags: HashMap<String, String>,
}

/// Submit the sandbox; returns its object id (the reattach handle).
pub async fn run_job(spec: &ModalJobSpec) -> Result<String> {
    // Merge stderr into stdout for the whole script so the single stdout stream
    // the launcher tails carries everything.
    let merged = format!("{{\n{}\n}} 2>&1", spec.script);
    // Default the sandbox's Python to unbuffered so the job's own prints stream
    // live instead of block-buffering behind a pipe (see jobs::default_unbuffered).
    let env = super::default_unbuffered(&spec.env);
    let body = json!({
        "app": spec.app,
        "image": spec.image,
        "script": merged,
        "gpu": spec.gpu,
        "cpu": spec.cpu,
        "memory": spec.memory,
        "env": env,
        "timeoutSeconds": spec.timeout_seconds,
        "tags": spec.tags,
    });
    let out = launcher_capture(&["submit"], Some(&body.to_string())).await?;
    let v: Value = serde_json::from_slice(&out).map_err(|e| {
        anyhow!(
            "Unreadable modal submit output: {} ({:?})",
            e,
            String::from_utf8_lossy(&out)
        )
    })?;
    v.get("sandboxId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("modal submit returned no sandboxId"))
}

/// Sandbox state in the shared stage vocabulary (see `jobs::stage_to_run_status`).
#[derive(Debug, Clone)]
pub struct JobState {
    pub stage: String,
    pub message: Option<String>,
}

pub async fn inspect_job(sandbox_id: &str) -> Result<JobState> {
    let out = launcher_capture(&["status", sandbox_id], None).await?;
    let v: Value = serde_json::from_slice(&out)
        .map_err(|e| anyhow!("Unreadable modal status output: {}", e))?;
    Ok(JobState {
        stage: v
            .get("stage")
            .and_then(Value::as_str)
            .unwrap_or("RUNNING")
            .to_string(),
        message: v.get("message").and_then(Value::as_str).map(str::to_string),
    })
}

pub async fn cancel_job(sandbox_id: &str) -> Result<()> {
    launcher_capture(&["cancel", sandbox_id], None).await?;
    Ok(())
}

/// One pass over the sandbox's log stream, invoking `sink` per line past `skip`.
///
/// Same replay/dedup contract as `hf::stream_logs` and `k8s::stream_logs`: the
/// launcher replays the whole stdout from the start on each connect, so the
/// caller passes how many lines it has consumed and gets the new total back.
/// Ends when the launcher exits (sandbox finished) or after `idle` silence.
pub async fn stream_logs(
    sandbox_id: &str,
    skip: u64,
    idle: Duration,
    sink: &mut (dyn FnMut(&str) + Send),
) -> Result<u64> {
    use tokio::io::{AsyncBufReadExt as _, BufReader};

    let mut child = Command::new(python_bin())
        .arg("-c")
        .arg(LAUNCHER)
        .args(["logs", sandbox_id])
        // Unbuffer the launcher's own stdout: we read it line-by-line, so block
        // buffering (stdout is a pipe here, not a TTY) would stall log streaming.
        .env(super::PYTHONUNBUFFERED, "1")
        .envs(modal_auth_env())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null()) // startup noise; state comes from inspect
        .spawn()
        .map_err(|e| missing_python_hint(&e))?;

    let stdout = child.stdout.take().expect("piped stdout");
    let mut lines = BufReader::new(stdout).lines();
    let mut seen = 0u64;
    loop {
        match tokio::time::timeout(idle, lines.next_line()).await {
            Err(_) => break,       // idle — let the caller re-check state
            Ok(Err(_)) => break,   // read error
            Ok(Ok(None)) => break, // launcher exited
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

/// Detected Modal readiness for the Settings/preflight surfaces.
pub struct ModalStatus {
    /// The orx-managed venv exists (whether or not `modal` imports cleanly).
    pub env_provisioned: bool,
    pub modal_importable: bool,
    pub token_configured: bool,
    /// Where the token came from: "env" | "syncedEnv" | "modalToml".
    pub token_source: Option<&'static str>,
    pub error: Option<String>,
}

/// Fail fast before doing any launch work (git push, api registration) if
/// Modal isn't usable — provisioning the orx-managed env on first use. Returns
/// a friendly, actionable error otherwise.
pub async fn preflight() -> Result<()> {
    // Build the managed venv on first launch (no-op once it exists).
    ensure_env().await?;
    let s = detect().await;
    if !s.modal_importable {
        return Err(anyhow!(
            "The `modal` SDK isn't importable by {} ({}). Remove {} to force a rebuild, or set \
             ORX_MODAL_PYTHON to an interpreter that has it.",
            python_bin(),
            s.error.as_deref().unwrap_or("import modal failed"),
            managed_env_dir().display()
        ));
    }
    if !s.token_configured {
        return Err(anyhow!(
            "No Modal token configured. Run `modal token new`, or set MODAL_TOKEN_ID and \
             MODAL_TOKEN_SECRET (in the environment, `orx up` Settings → Environment, or \
             `~/.openresearch/env`)."
        ));
    }
    Ok(())
}

/// Best-effort probe: is a Modal interpreter present, is `modal` importable, and
/// is a token configured (process env, the synced env file, or `~/.modal.toml`)?
pub async fn detect() -> ModalStatus {
    let token_source = if std::env::var("MODAL_TOKEN_ID").is_ok_and(|t| !t.trim().is_empty()) {
        Some("env")
    } else if crate::config::synced_env_var("MODAL_TOKEN_ID").is_some() {
        Some("syncedEnv")
    } else if dirs::home_dir().is_some_and(|h| h.join(".modal.toml").exists()) {
        Some("modalToml")
    } else {
        None
    };
    let token_configured = token_source.is_some();
    let env_provisioned = managed_python().exists();
    let mk = |modal_importable, error| ModalStatus {
        env_provisioned,
        modal_importable,
        token_configured,
        token_source,
        error,
    };
    let probe = Command::new(python_bin())
        .arg("-c")
        .arg("import modal")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await;
    match probe {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            mk(false, Some(format!("{} not found on PATH", python_bin())))
        }
        Err(e) => mk(false, Some(e.to_string())),
        Ok(out) if out.status.success() => mk(true, None),
        Ok(out) => mk(
            false,
            Some(
                String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .last()
                    .unwrap_or("`import modal` failed")
                    .to_string(),
            ),
        ),
    }
}
