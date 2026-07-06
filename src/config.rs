//! Credential storage, XDG paths, and the default API URL.
//!
//! Credentials live at
//! `$XDG_CONFIG_HOME/openresearch/credentials.json` (falling back to
//! `~/.config/openresearch/credentials.json`), written owner-only (mode 0600).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::error::Result;

/// Base URL of the API. Defaults to prod (`https://api.openresearch.sh`), so a
/// plain `orx login` just works. Override per-invocation with `--api-url` (handled
/// in the `login` command) or set the `OPENRESEARCH_API_URL` env var to point at
/// local dev, e.g. `OPENRESEARCH_API_URL=http://localhost:4000 orx login`.
pub fn default_api_url() -> String {
    std::env::var("OPENRESEARCH_API_URL")
        .unwrap_or_else(|_| "https://api.openresearch.sh".to_string())
}

/// Base URL for the alphaXiv JSON API (full-text literature search). Unlike the
/// OpenResearch API these endpoints are public — no token — and live on a
/// different host. Override with `ALPHAXIV_API_URL`.
pub fn alphaxiv_api_url() -> String {
    std::env::var("ALPHAXIV_API_URL").unwrap_or_else(|_| "https://api.alphaxiv.org".to_string())
}

/// Base URL for the alphaXiv web app, which serves the per-paper `.md` routes
/// (`/overview/<id>.md` report, `/abs/<id>.md` full text). Default is the `www.`
/// host so we skip the apex→www 301. Override with `ALPHAXIV_WEB_URL`.
pub fn alphaxiv_web_url() -> String {
    std::env::var("ALPHAXIV_WEB_URL").unwrap_or_else(|_| "https://www.alphaxiv.org".to_string())
}

/// Stored credentials: the API base URL and the bearer token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    #[serde(rename = "apiUrl")]
    pub api_url: String,
    pub token: String,
}

pub(crate) fn config_dir() -> PathBuf {
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

/// Look up a var from the box's synced env file (`~/.openresearch/env`, written
/// by the api's env sync). Needed because non-interactive shells never source
/// it via .bashrc (Ubuntu's interactive guard returns first), so an agent's
/// `orx` can't rely on the process environment alone. Parses only the exact
/// format the api writes: `export KEY='value'` with `\` doubled and `'`
/// written as `'\''`.
pub fn synced_env_var(key: &str) -> Option<String> {
    let path = dirs::home_dir()?.join(".openresearch").join("env");
    let content = std::fs::read_to_string(path).ok()?;
    let prefix = format!("export {key}='");
    for line in content.lines() {
        let Some(rest) = line.strip_prefix(&prefix) else {
            continue;
        };
        let Some(escaped) = rest.strip_suffix('\'') else {
            continue;
        };
        // Invert buildEnvFile's escaping (quotes first, then backslashes).
        let value = escaped.replace(r"'\''", "'").replace(r"\\", r"\");
        if !value.is_empty() {
            return Some(value);
        }
    }
    None
}

/// All vars in the synced env file, in file order (same format as
/// `synced_env_var`). Malformed lines are skipped.
pub fn list_synced_env() -> Vec<(String, String)> {
    let Some(path) = dirs::home_dir().map(|h| h.join(".openresearch").join("env")) else {
        return Vec::new();
    };
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in content.lines() {
        let Some(rest) = line.strip_prefix("export ") else {
            continue;
        };
        let Some((key, quoted)) = rest.split_once('=') else {
            continue;
        };
        let Some(escaped) = quoted.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) else {
            continue;
        };
        let value = escaped.replace(r"'\''", "'").replace(r"\\", r"\");
        if !key.is_empty() && !value.is_empty() {
            out.push((key.to_string(), value));
        }
    }
    out
}

/// Drop `key`'s line from the synced env file. Missing file/key is a no-op.
pub fn remove_synced_env_var(key: &str) -> Result<()> {
    let Some(path) = dirs::home_dir().map(|h| h.join(".openresearch").join("env")) else {
        return Ok(());
    };
    let Ok(existing) = std::fs::read_to_string(&path) else {
        return Ok(());
    };
    let prefix = format!("export {key}=");
    let lines: Vec<&str> = existing
        .lines()
        .filter(|l| !l.starts_with(&prefix))
        .collect();
    let body = if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    };
    std::fs::write(&path, body)?;
    Ok(())
}

/// Write `export KEY='value'` into `~/.openresearch/env` (the exact format
/// `synced_env_var` parses), replacing an existing line for `key` and keeping
/// every other line. File is owner-only (0600) on create and rewrite.
pub fn write_synced_env_var(key: &str, value: &str) -> Result<()> {
    use anyhow::anyhow;
    let dir = dirs::home_dir()
        .ok_or_else(|| anyhow!("no home directory"))?
        .join(".openresearch");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("env");
    // Inverse of synced_env_var's unescaping: backslashes first, then quotes.
    let escaped = value.replace('\\', r"\\").replace('\'', r"'\''");
    let new_line = format!("export {key}='{escaped}'");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let prefix = format!("export {key}=");
    let mut lines: Vec<&str> = Vec::new();
    let mut replaced = false;
    for line in existing.lines() {
        if line.starts_with(&prefix) {
            if !replaced {
                lines.push(&new_line);
                replaced = true;
            }
        } else {
            lines.push(line);
        }
    }
    if !replaced {
        lines.push(&new_line);
    }
    let body = format!("{}\n", lines.join("\n"));
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600); // applies on create only
        }
        opts.open(&path)?.write_all(body.as_bytes())?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
