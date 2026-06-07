//! Credential storage, XDG paths, and the default API URL.
//!
//! Credentials live at
//! `$XDG_CONFIG_HOME/openresearch/credentials.json` (falling back to
//! `~/.config/openresearch/credentials.json`), written owner-only (mode 0600).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::error::Result;

/// Base URL of the API. Override per-invocation with `--api-url` (handled in the
/// `login` command) or persist via the `OPENRESEARCH_API_URL` env var. Defaults
/// to local dev.
pub fn default_api_url() -> String {
    std::env::var("OPENRESEARCH_API_URL").unwrap_or_else(|_| "http://localhost:4000".to_string())
}

/// Stored credentials: the API base URL and the bearer token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    #[serde(rename = "apiUrl")]
    pub api_url: String,
    pub token: String,
}

fn config_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
        });
    base.join("openresearch")
}

fn credentials_path() -> PathBuf {
    config_dir().join("credentials.json")
}

/// Reads stored credentials. Returns `Ok(None)` when the file is missing,
/// unreadable, malformed, or missing required fields — matching the TS
/// `loadCredentials`, which swallows all errors and returns `null`.
pub async fn load_credentials() -> Result<Option<Credentials>> {
    let path = credentials_path();
    let raw = match fs::read_to_string(&path).await {
        Ok(raw) => raw,
        Err(_) => return Ok(None),
    };
    match serde_json::from_str::<Credentials>(&raw) {
        Ok(creds) if !creds.api_url.is_empty() && !creds.token.is_empty() => Ok(Some(creds)),
        _ => Ok(None),
    }
}

/// Persists credentials as pretty JSON with a trailing newline, mode 0600.
pub async fn save_credentials(creds: &Credentials) -> Result<()> {
    let path = credentials_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let body = format!("{}\n", serde_json::to_string_pretty(creds)?);
    fs::write(&path, body).await?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        fs::set_permissions(&path, perms).await?;
    }

    Ok(())
}

/// Removes the credentials file. Succeeds even if it does not exist (`force`).
pub async fn clear_credentials() -> Result<()> {
    match fs::remove_file(credentials_path()).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
