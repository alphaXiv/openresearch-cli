//! Self-update plumbing shared by `orx version`, `orx update`, and the passive
//! update nudge.
//!
//! Latest-version discovery deliberately avoids the GitHub REST API: its
//! unauthenticated limit is 60 requests/hour *per IP*, which is routinely
//! exhausted on the datacenter/NAT addresses agents run from. Instead we fetch
//! the `dist-manifest.json` asset that cargo-dist uploads to every release via
//! the documented `releases/latest/download/<asset>` permalink — a plain CDN
//! redirect with no API rate limit.
//!
//! The cargo-dist shell installer writes an install receipt to
//! `${XDG_CONFIG_HOME:-~/.config}/openresearch-cli/openresearch-cli-receipt.json`.
//! That receipt is the only thing distinguishing an installer-managed binary
//! from a `cargo install` one (both live at `~/.cargo/bin/orx` because
//! dist-workspace.toml sets `install-path = "CARGO_HOME"`), so `orx update`
//! refuses to touch the binary unless the receipt matches it.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::error::{anyhow, Result};

/// GitHub repo the released binaries come from.
pub const REPO_URL: &str = "https://github.com/alphaXiv/openresearch-cli";

/// The cargo-dist app name (the *package* name, not the `orx` bin name) — used
/// in release asset names and the receipt path.
pub const APP_NAME: &str = "openresearch-cli";

/// How long a cached update check stays fresh.
const CHECK_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Sent on requests to GitHub — some CDNs reject the default (empty) UA.
const UA: &str = concat!("openresearch-cli/", env!("CARGO_PKG_VERSION"));

pub fn current_version() -> Version {
    // The crate version is always valid semver; a panic here is a build bug.
    Version::parse(env!("CARGO_PKG_VERSION")).expect("CARGO_PKG_VERSION is valid semver")
}

fn http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

// ---------------------------------------------------------------------------
// Latest-version discovery (dist-manifest.json)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct LatestRelease {
    pub version: Version,
    /// The git tag (e.g. `v0.1.15`), used to pin asset downloads to the same
    /// release the manifest described.
    pub tag: String,
}

#[derive(Deserialize)]
struct DistManifest {
    announcement_tag: String,
    #[serde(default)]
    releases: Vec<ManifestRelease>,
}

#[derive(Deserialize)]
struct ManifestRelease {
    app_name: String,
    app_version: String,
}

/// Extracts our app's version (and the release tag) from a dist-manifest body.
fn parse_manifest(body: &str) -> Result<LatestRelease> {
    let manifest: DistManifest = serde_json::from_str(body)?;
    let release = manifest
        .releases
        .iter()
        .find(|r| r.app_name == APP_NAME)
        .ok_or_else(|| anyhow!("Release manifest has no entry for {}", APP_NAME))?;
    let version = Version::parse(&release.app_version)
        .map_err(|e| anyhow!("Could not parse version {:?}: {}", release.app_version, e))?;
    Ok(LatestRelease {
        version,
        tag: manifest.announcement_tag,
    })
}

/// Fetches the latest released version from GitHub (rate-limit-free permalink).
pub async fn fetch_latest(timeout: Duration) -> Result<LatestRelease> {
    let url = format!("{}/releases/latest/download/dist-manifest.json", REPO_URL);
    let res = http()
        .get(&url)
        .header("user-agent", UA)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| {
            anyhow!(
                "Could not fetch the release manifest from {}: {}",
                REPO_URL,
                e
            )
        })?;
    let status = res.status();
    if !status.is_success() {
        let reason = status.canonical_reason().unwrap_or("");
        return Err(anyhow!(
            "Release manifest request failed ({} {})",
            status.as_u16(),
            reason
        ));
    }
    let body = res.text().await?;
    parse_manifest(&body)
}

/// Downloads a release asset pinned to `tag` and returns its bytes.
pub async fn fetch_release_asset(tag: &str, asset: &str, timeout: Duration) -> Result<Vec<u8>> {
    let url = format!("{}/releases/download/{}/{}", REPO_URL, tag, asset);
    let res = http()
        .get(&url)
        .header("user-agent", UA)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| anyhow!("Could not download {}: {}", url, e))?;
    let status = res.status();
    if !status.is_success() {
        let reason = status.canonical_reason().unwrap_or("");
        return Err(anyhow!(
            "Download of {} failed ({} {})",
            url,
            status.as_u16(),
            reason
        ));
    }
    Ok(res.bytes().await?.to_vec())
}

// ---------------------------------------------------------------------------
// Install receipt (written by the cargo-dist shell installer)
// ---------------------------------------------------------------------------

/// The fields of the cargo-dist install receipt that `orx update` relies on.
#[derive(Debug, Deserialize)]
pub struct Receipt {
    pub install_prefix: String,
    pub version: String,
    #[serde(default)]
    pub modify_path: bool,
}

pub fn receipt_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
        });
    base.join(APP_NAME)
        .join(format!("{}-receipt.json", APP_NAME))
}

/// Reads the install receipt. `Ok(None)` when it does not exist (i.e. the
/// shell installer never ran on this machine); `Err` when it exists but cannot
/// be parsed, since silently treating a corrupt receipt as "not installed by
/// the installer" would point users at the wrong update path.
pub fn load_receipt() -> Result<Option<Receipt>> {
    let path = receipt_path();
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow!("Could not read {}: {}", path.display(), e)),
    };
    let receipt: Receipt = serde_json::from_str(&raw)
        .map_err(|e| anyhow!("Install receipt at {} is malformed: {}", path.display(), e))?;
    Ok(Some(receipt))
}

/// Whether the running executable lives under the receipt's install prefix.
/// The receipt records the prefix root (e.g. `~/.cargo`), while the binary sits
/// in its `bin/` subdirectory for the cargo-home layout — so a trailing `bin`
/// component is stripped from the exe's directory before comparing. Both paths
/// should be canonicalized by the caller.
pub fn exe_matches_prefix(exe: &Path, prefix: &Path) -> bool {
    let Some(dir) = exe.parent() else {
        return false;
    };
    let dir = if dir.file_name() == Some(std::ffi::OsStr::new("bin")) {
        dir.parent().unwrap_or(dir)
    } else {
        dir
    };
    dir == prefix
}

// ---------------------------------------------------------------------------
// Update-check cache (the passive nudge's 24h throttle)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct CheckCache {
    /// Unix seconds of the last completed (or attempted) check.
    checked_at: u64,
    /// Latest version seen at that time.
    latest: String,
}

fn cache_path() -> PathBuf {
    crate::config::config_dir().join("update-check.json")
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_cache() -> Option<CheckCache> {
    let raw = std::fs::read_to_string(cache_path()).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Best-effort cache write; errors are swallowed because the cache only exists
/// to throttle a best-effort background check.
pub fn write_check_cache(latest: &str) {
    let cache = CheckCache {
        checked_at: now_unix(),
        latest: latest.to_string(),
    };
    let path = cache_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(body) = serde_json::to_string(&cache) {
        let _ = std::fs::write(&path, body);
    }
}

// ---------------------------------------------------------------------------
// Passive nudge
// ---------------------------------------------------------------------------

/// Whether the passive update check may run at all. Interactive terminals
/// only: agents, scripts, pipes, and CI see zero behavior change.
fn nudge_enabled() -> bool {
    let opted_out = std::env::var_os("ORX_NO_UPDATE_CHECK").is_some()
        // The generic convention honored by update-notifier and friends.
        || std::env::var_os("NO_UPDATE_NOTIFIER").is_some()
        // cargo-dist's own "don't manage updates for this install" switch; the
        // installer already honors it, so it is the one mental model.
        || std::env::var("OPENRESEARCH_CLI_DISABLE_UPDATE").as_deref() == Ok("1")
        || std::env::var_os("CI").is_some();
    !opted_out && std::io::stderr().is_terminal()
}

/// The passive update nudge, modeled on the gh CLI / update-notifier pattern:
/// the message shown this run comes from the *cached* previous check (so it is
/// instant), and a background refresh updates the cache for the next run at
/// most once per [`CHECK_TTL`].
pub struct Nudge {
    message: Option<String>,
    refresh: Option<tokio::task::JoinHandle<()>>,
}

impl Nudge {
    pub fn start() -> Nudge {
        if !nudge_enabled() {
            return Nudge {
                message: None,
                refresh: None,
            };
        }

        let current = current_version();
        let cache = read_cache();

        let message = cache
            .as_ref()
            .and_then(|c| Version::parse(&c.latest).ok())
            .filter(|latest| *latest > current)
            .map(|latest| {
                format!(
                    "A new release of orx is available: {} → {}. Run `orx update` to upgrade.",
                    current, latest
                )
            });

        let stale = cache
            .as_ref()
            .map(|c| now_unix().saturating_sub(c.checked_at) >= CHECK_TTL.as_secs())
            .unwrap_or(true);
        let refresh = stale.then(|| {
            let prev_latest = cache.map(|c| c.latest);
            tokio::spawn(async move {
                let latest = fetch_latest(Duration::from_secs(3))
                    .await
                    .ok()
                    .map(|l| l.version.to_string());
                // On fetch failure, refresh checked_at with the old answer so
                // errors don't cause a retry on every invocation.
                let value = latest
                    .or(prev_latest)
                    .unwrap_or_else(|| current.to_string());
                write_check_cache(&value);
            })
        });

        Nudge { message, refresh }
    }

    /// Called after the real command finished: gives the background refresh a
    /// short grace window (it usually finished while the command did its own
    /// network work), then prints the nudge — stderr only, exit code untouched.
    pub async fn finish(self) {
        if let Some(handle) = self.refresh {
            if tokio::time::timeout(Duration::from_millis(250), handle)
                .await
                .is_err()
            {
                // Took too long; the next stale run will try again.
            }
        }
        if let Some(message) = self.message {
            eprintln!("\n{}", message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{exe_matches_prefix, parse_manifest};
    use semver::Version;
    use std::path::Path;

    #[test]
    fn parses_dist_manifest() {
        let body = r#"{
            "dist_version": "0.32.0",
            "announcement_tag": "v0.1.15",
            "announcement_is_prerelease": false,
            "releases": [{
                "app_name": "openresearch-cli",
                "app_version": "0.1.15",
                "artifacts": ["openresearch-cli-installer.sh"]
            }]
        }"#;
        let latest = parse_manifest(body).unwrap();
        assert_eq!(latest.tag, "v0.1.15");
        assert_eq!(latest.version, Version::new(0, 1, 15));
    }

    #[test]
    fn manifest_without_our_app_is_an_error() {
        let body = r#"{"announcement_tag": "v1.0.0", "releases": [{"app_name": "other", "app_version": "1.0.0"}]}"#;
        assert!(parse_manifest(body).is_err());
    }

    #[test]
    fn semver_ordering_not_lexicographic() {
        // The case string comparison gets wrong.
        assert!(Version::parse("0.1.15").unwrap() > Version::parse("0.1.9").unwrap());
    }

    #[test]
    fn exe_prefix_matching() {
        let cases = [
            // cargo-home layout: receipt prefix is the root, binary in bin/.
            ("/home/u/.cargo/bin/orx", "/home/u/.cargo", true),
            // flat layout: binary directly in the prefix.
            ("/opt/orx/orx", "/opt/orx", true),
            // foreign binary elsewhere on PATH.
            ("/usr/local/bin/orx", "/home/u/.cargo", false),
        ];
        for (exe, prefix, want) in cases {
            assert_eq!(
                exe_matches_prefix(Path::new(exe), Path::new(prefix)),
                want,
                "exe={} prefix={}",
                exe,
                prefix
            );
        }
    }
}
