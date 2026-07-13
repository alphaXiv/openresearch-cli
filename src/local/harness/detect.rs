//! Shared detection primitives for the harness registry — the wire types every
//! harness reports (`HarnessInfo`, `ModelInfo`) and the best-effort probes
//! (`--version`, auth-file reads, JWT decode) the per-harness impls build on.
//!
//! Detection is read-only and best-effort: missing files or unparseable JSON
//! just mean "not detected", never an error.

use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

pub(super) const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessInfo {
    pub id: &'static str,
    pub name: &'static str,
    pub installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bin_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// A signed-in setup was found (auth file / OAuth account).
    pub authenticated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<&'static str>, // "oauth" | "apiKey"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Usable as a chat backend right now (installed + signed in).
    pub agent_ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_note: Option<String>,
    pub models: Vec<ModelInfo>,
    /// Composer toggle vocabulary (permission modes, reasoning levels).
    pub options: super::HarnessOptions,
}

impl HarnessInfo {
    pub(super) fn new(id: &'static str, name: &'static str) -> Self {
        Self {
            id,
            name,
            installed: false,
            bin_path: None,
            version: None,
            authenticated: false,
            auth_method: None,
            account: None,
            org: None,
            plan: None,
            agent_ready: false,
            agent_note: None,
            models: Vec::new(),
            options: super::HarnessOptions::none(),
        }
    }

    /// Attach the chat model list from a set of static ids.
    pub(super) fn with_models(mut self, ids: &[&str]) -> Self {
        self.models = ids
            .iter()
            .map(|id| ModelInfo { id: id.to_string() })
            .collect();
        self
    }
}

pub(super) fn find_on_path(bin: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(bin))
        .find(|c| c.is_file())
}

/// Dereference symlinks to the real installed binary. Installers commonly drop
/// a lone symlink into `~/.local/bin`, but some CLIs locate sibling helper
/// executables relative to the path they were *invoked as*, without resolving
/// symlinks — codex >= 0.144 launches `codex-code-mode-host` this way and every
/// command fails with "No such file or directory" when codex is spawned via the
/// symlink. Spawning the resolved path keeps helpers real siblings. Best-effort:
/// a path that can't be resolved is returned unchanged.
pub(super) fn resolve_symlinks(path: PathBuf) -> PathBuf {
    path.canonicalize().unwrap_or(path)
}

/// `<bin> --version`, first line, with a timeout (node CLIs can be slow).
pub(super) async fn bin_version(bin: &PathBuf) -> Option<String> {
    let fut = tokio::process::Command::new(bin)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output();
    let out = tokio::time::timeout(VERSION_TIMEOUT, fut)
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()?
        .trim()
        .to_string();
    (!line.is_empty()).then_some(line)
}

pub(super) fn read_json(path: PathBuf) -> Option<Value> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

pub(super) fn nonempty_str(v: &Value, key: &str) -> Option<String> {
    v.get(key)?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Decode a JWT's payload without verifying — we only surface the account
/// email and plan the user is already signed in as, locally.
pub(super) fn jwt_payload(token: &str) -> Option<Value> {
    use base64::Engine as _;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub(super) fn title_case(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn resolve_symlinks_dereferences_to_real_binary() {
        let dir = std::env::temp_dir().join(format!("orx-detect-test-{}", std::process::id()));
        let install = dir.join("install");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&install).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let real = install.join("codex");
        std::fs::write(&real, "").unwrap();
        let link = bin.join("codex");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert_eq!(resolve_symlinks(link), real.canonicalize().unwrap());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_symlinks_keeps_unresolvable_path() {
        let missing = PathBuf::from("/nonexistent/orx-detect-test/codex");
        assert_eq!(resolve_symlinks(missing.clone()), missing);
    }
}
