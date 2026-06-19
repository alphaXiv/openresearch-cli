//! Self-update plumbing shared by `orx version`, `orx update`, and the passive
//! outdated-version warning.
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

use std::io::{IsTerminal, Write};
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
// Update-check cache (the warning's 24h throttle)
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
///
/// The write is atomic (temp file + rename) so a reader never observes a torn
/// file. Several processes can write this concurrently — the background refresh
/// here plus `orx version` / `orx update`, and multiple `orx` invocations at
/// once — and a non-atomic `write` could leave a half-written file that
/// `read_cache` then parses to `None`, silently suppressing the warning until a
/// clean write lands. A unique temp name keeps concurrent writers from
/// clobbering each other's temp file; `rename` is atomic on the same
/// filesystem, so the final file is always either the old or a complete new one.
pub fn write_check_cache(latest: &str) {
    let cache = CheckCache {
        checked_at: now_unix(),
        latest: latest.to_string(),
    };
    let path = cache_path();
    let Some(parent) = path.parent() else {
        return;
    };
    let _ = std::fs::create_dir_all(parent);
    let Ok(body) = serde_json::to_string(&cache) else {
        return;
    };
    let tmp = parent.join(format!(".update-check.json.{}.tmp", uuid::Uuid::new_v4()));
    if std::fs::write(&tmp, body).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    if std::fs::rename(&tmp, &path).is_err() {
        // Rename failed (e.g. cross-device, racing cleanup); don't leak the temp.
        let _ = std::fs::remove_file(&tmp);
    }
}

// ---------------------------------------------------------------------------
// Passive outdated-version warning
// ---------------------------------------------------------------------------

/// Whether the user has explicitly silenced the update check. When true, no
/// warning is shown and no background refresh runs — the one escape hatch for
/// anyone who can't tolerate the extra stderr line (including CI).
fn opted_out() -> bool {
    std::env::var_os("ORX_NO_UPDATE_CHECK").is_some()
        // The generic convention honored by update-notifier and friends.
        || std::env::var_os("NO_UPDATE_NOTIFIER").is_some()
        // cargo-dist's own "don't manage updates for this install" switch; the
        // installer already honors it, so it is the one mental model.
        || std::env::var("OPENRESEARCH_CLI_DISABLE_UPDATE").as_deref() == Ok("1")
}

/// Whether to emit ANSI styling on stderr: only when stderr is a real terminal
/// and the user hasn't set the conventional `NO_COLOR` opt-out. Pipes, files,
/// and CI logs get plain text — never raw escape codes — which matters because
/// this warning now prints on non-interactive runs too.
fn stderr_supports_ansi() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal()
}

/// Wraps `text` in the ANSI bold sequence when `enabled`, else returns it as-is.
/// `\x1b[22m` resets bold/faint specifically (not `\x1b[0m`, which would also
/// clear any surrounding styling the terminal had).
fn bold(text: &str, enabled: bool) -> String {
    if enabled {
        format!("\x1b[1m{text}\x1b[22m")
    } else {
        text.to_string()
    }
}

/// Renders the warning for printing to stderr: bolds the leading `Warning:`
/// label when `ansi` is set, and sets the whole thing off in its own block with
/// a blank line above and below. Returns the exact bytes to write (the caller
/// uses `write!`, not `writeln!`, since the trailing newlines are included).
fn render(message: &str, ansi: bool) -> String {
    // The message always begins with the literal "Warning:" (see `warning_for`);
    // bold just that label, gh/cargo-style, rather than the whole sentence.
    const LABEL: &str = "Warning:";
    let styled = match message.strip_prefix(LABEL) {
        Some(rest) => format!("{}{rest}", bold(LABEL, ansi)),
        None => message.to_string(),
    };
    format!("\n{styled}\n\n")
}

/// Whether a `current` → `latest` upgrade crosses a breaking-change boundary
/// under semver. A major bump is always breaking; pre-1.0 (`0.y.z`) a *minor*
/// bump is the breaking signal too, since `0.y` versions treat `y` like a major
/// (`0.0.z` then treats every `z` as breaking). Post-1.0 a minor bump is
/// additive. Assumes `latest` outranks `current` in [`precedence`] (the only
/// case the warning builds for).
///
/// RE-CONFIRM AT 1.0: the moment orx ships `1.0.0`, a `1.x` minor bump silently
/// downgrades from breaking to soft here. That is semver-correct, but if the
/// backend keeps making breaking changes on minor bumps post-1.0, this rule
/// would understate them — revisit the heuristic then.
fn is_breaking_gap(current: &Version, latest: &Version) -> bool {
    if latest.major != current.major {
        return latest.major > current.major;
    }
    // Same major. Below 1.0 the minor (and, below 0.1, the patch) is the
    // breaking axis.
    if current.major == 0 {
        if current.minor == 0 {
            return latest.minor > 0 || latest.patch > current.patch;
        }
        return latest.minor > current.minor;
    }
    false
}

/// Version-precedence key: everything that orders two releases *except* build
/// metadata. Per the SemVer spec build metadata is not part of precedence
/// (`1.0.0+a` and `1.0.0+b` are the same release), but `semver::Version`'s `Ord`
/// compares it anyway — so two builds of the same release would otherwise read
/// as "outdated". Comparing this key instead avoids that false positive.
/// `Prerelease`'s own `Ord` already encodes the spec rule that a release sorts
/// above its pre-releases (empty pre-release is the greatest).
fn precedence(v: &Version) -> (u64, u64, u64, &semver::Prerelease) {
    (v.major, v.minor, v.patch, &v.pre)
}

/// Builds the outdated-version warning when `latest` outranks `current` in
/// [`precedence`], or `None` when `current` is already current (or ahead — a
/// local dev build, or the same release with different build metadata).
///
/// Because orx talks to a versioned backend, a stale client can hit removed or
/// changed API shapes, so the warning leads with that risk rather than a
/// neutral "new version available". The framing escalates with the size of the
/// gap (see [`is_breaking_gap`]): a breaking gap warns that commands can start
/// failing now, while a non-breaking gap is a gentler "upgrade soon".
fn warning_for(current: &Version, latest: &Version) -> Option<String> {
    if precedence(latest) <= precedence(current) {
        return None;
    }
    let detail = if is_breaking_gap(current, latest) {
        "This release may include breaking API changes, so commands can start failing \
         until you upgrade."
    } else {
        "A newer release is available; upgrade to stay compatible with the API."
    };
    Some(format!(
        "Warning: orx {current} is outdated (latest {latest}). {detail} \
         Run `orx update` to upgrade."
    ))
}

/// The passive outdated-version warning, modeled on the gh CLI / update-notifier
/// pattern: the message shown this run comes from the *cached* previous check,
/// so it is instant and never adds latency to the command. A background refresh
/// updates the cache for the next run at most once per [`CHECK_TTL`].
///
/// Unlike a quiet "new version" nudge, the warning is shown on every command —
/// piped, scripted, or interactive — and is printed in [`start`](Self::start),
/// *before* the command runs, so it survives commands that `std::process::exit`
/// on their own (e.g. the "not logged in" path) instead of returning to `main`.
/// [`opted_out`] is the single switch to silence it.
///
/// Because the message comes from cache, the *first* run after a fresh install
/// has nothing cached yet and shows nothing; the background refresh it kicks off
/// warms the cache so a later run can warn. The refresh is always fire-and-forget
/// — it never blocks the command — so a one-shot environment that throws its
/// cache away each run (e.g. an ephemeral CI container) may take a few runs to
/// warm up, or never. That is the accepted cost of adding zero latency.
pub struct UpdateWarning {
    refresh: Option<tokio::task::JoinHandle<()>>,
}

impl UpdateWarning {
    /// Prints the cached warning (if any) to stderr immediately, then kicks off a
    /// best-effort background refresh of the cached "latest" for next time.
    /// Printing here — rather than after the command — is what guarantees the
    /// warning shows even when the command exits the process itself.
    pub fn start() -> UpdateWarning {
        if opted_out() {
            return UpdateWarning { refresh: None };
        }

        let current = current_version();
        let cache = read_cache();

        if let Some(message) = cache
            .as_ref()
            .and_then(|c| Version::parse(&c.latest).ok())
            .and_then(|latest| warning_for(&current, &latest))
        {
            // Infallible: a closed/broken stderr (e.g. `2>&-`, or a reader that
            // already exited) must not panic the process before the command even
            // runs. `eprintln!` would; a swallowed `writeln!` won't.
            let _ = write!(
                std::io::stderr(),
                "{}",
                render(&message, stderr_supports_ansi())
            );
        }

        // Refresh whenever the cache is stale (or absent), regardless of whether
        // stderr is a terminal: scripted/piped runs warn too, so their cache has
        // to stay fresh or they'd warn off a frozen answer indefinitely.
        let stale = cache
            .as_ref()
            .map(|c| now_unix().saturating_sub(c.checked_at) >= CHECK_TTL.as_secs())
            .unwrap_or(true);
        if !stale {
            return UpdateWarning { refresh: None };
        }

        let prev_latest = cache.map(|c| c.latest);
        let handle = tokio::spawn(async move {
            let latest = fetch_latest(Duration::from_secs(3))
                .await
                .ok()
                .map(|l| l.version.to_string());
            // On fetch failure, refresh checked_at with the old answer so errors
            // don't cause a retry on every invocation. Only fabricate `current`
            // as a last resort (no cache, no previous answer): a one-off failed
            // fetch then suppresses the warning until the TTL lapses, which is
            // the price of not re-fetching on every offline invocation.
            let value = latest
                .or(prev_latest)
                .unwrap_or_else(|| current.to_string());
            write_check_cache(&value);
        });

        UpdateWarning {
            refresh: Some(handle),
        }
    }

    /// Called after the real command finished. Grants the fire-and-forget refresh
    /// a brief grace window to land in the cache — it has often finished during
    /// the command's own work — then moves on. The window matters most for a fast
    /// command (one that returns before the fetch does): it gives the write a
    /// chance to commit before `#[tokio::main]` tears the runtime down, without
    /// which quick commands would never warm the cache. Never blocks the command
    /// meaningfully, never touches stdout or the exit code.
    pub async fn finish(self) {
        let Some(handle) = self.refresh else {
            return;
        };
        // Timed out -> the task keeps running (it may still land); the next stale
        // run tries again regardless.
        let _ = tokio::time::timeout(Duration::from_millis(250), handle).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        bold, exe_matches_prefix, is_breaking_gap, parse_manifest, precedence, render, warning_for,
    };
    use semver::Version;
    use std::path::Path;

    #[test]
    fn render_sets_off_the_warning_in_its_own_block() {
        let msg = "Warning: orx 0.1.0 is outdated (latest 0.2.0). foo Run `orx update` to upgrade.";
        // Plain (non-TTY): blank line before and after, no escape codes at all.
        let plain = render(msg, false);
        assert_eq!(plain, format!("\n{msg}\n\n"));
        assert!(
            !plain.contains('\x1b'),
            "plain output must not contain ANSI"
        );

        // Styled (TTY): only the "Warning:" label is bolded; same blank-line block.
        let styled = render(msg, true);
        assert!(styled.starts_with("\n\x1b[1mWarning:\x1b[22m"));
        assert!(styled.ends_with("upgrade.\n\n"));
        // Everything after the label is unstyled — exactly one bold open/close.
        assert_eq!(styled.matches("\x1b[1m").count(), 1);
        assert_eq!(styled.matches("\x1b[22m").count(), 1);

        // Fail-soft: a message without the "Warning:" prefix is passed through
        // unstyled (no panic, no stray escapes), so the label/builder coupling
        // degrades gracefully if the prefix ever changes.
        let no_label = render("orx is outdated.", true);
        assert_eq!(no_label, "\norx is outdated.\n\n");
    }

    #[test]
    fn bold_is_a_noop_when_disabled() {
        assert_eq!(bold("x", false), "x");
        assert_eq!(bold("x", true), "\x1b[1mx\x1b[22m");
    }

    #[test]
    fn precedence_ignores_build_metadata_and_orders_prereleases() {
        let v = |s: &str| Version::parse(s).unwrap();
        // Build metadata is not part of precedence: same release.
        assert_eq!(precedence(&v("1.2.3+a")), precedence(&v("1.2.3+b")));
        assert_eq!(precedence(&v("1.2.3")), precedence(&v("1.2.3+build.99")));
        // A release outranks its pre-releases (empty pre-release is greatest).
        assert!(precedence(&v("0.2.0")) > precedence(&v("0.2.0-rc.1")));
        assert!(precedence(&v("0.2.0-rc.2")) > precedence(&v("0.2.0-rc.1")));
        // Ordinary ordering on the numeric fields still holds.
        assert!(precedence(&v("0.2.0")) > precedence(&v("0.1.29")));
    }

    // The substring the *breaking* message has and the soft one must not, used
    // to assert which tier a warning landed in without pinning exact copy.
    const BREAKING_MARKER: &str = "can start failing";

    fn assert_soft(msg: &str) {
        assert!(msg.contains("outdated"), "{msg}");
        assert!(msg.contains("orx update"), "{msg}");
        assert!(
            !msg.contains(BREAKING_MARKER),
            "soft warning claims breakage: {msg}"
        );
    }

    fn assert_breaking(msg: &str) {
        assert!(msg.contains("breaking API changes"), "{msg}");
        assert!(msg.contains(BREAKING_MARKER), "{msg}");
        assert!(msg.contains("orx update"), "{msg}");
    }

    #[test]
    fn no_warning_when_current_or_ahead() {
        let v = |s: &str| Version::parse(s).unwrap();
        // Exactly current.
        assert!(warning_for(&v("0.1.29"), &v("0.1.29")).is_none());
        // Local build ahead of the latest release.
        assert!(warning_for(&v("0.2.0"), &v("0.1.29")).is_none());
        // Build metadata is ignored by semver ordering, so it's not "outdated".
        assert!(warning_for(&v("0.1.29"), &v("0.1.29+build.5")).is_none());
    }

    #[test]
    fn is_breaking_gap_classifies_every_axis() {
        let v = |s: &str| Version::parse(s).unwrap();
        // major bump: always breaking.
        assert!(is_breaking_gap(&v("0.9.0"), &v("1.0.0")));
        assert!(is_breaking_gap(&v("1.4.0"), &v("2.0.0")));
        // pre-1.0 minor bump: breaking.
        assert!(is_breaking_gap(&v("0.1.29"), &v("0.2.0")));
        // pre-1.0 patch bump: NOT breaking.
        assert!(!is_breaking_gap(&v("0.1.28"), &v("0.1.29")));
        // 0.0.x: even a patch bump is breaking (every 0.0.z is its own "major").
        assert!(is_breaking_gap(&v("0.0.1"), &v("0.0.2")));
        // post-1.0 minor/patch bump: NOT breaking (additive).
        assert!(!is_breaking_gap(&v("1.2.3"), &v("1.3.0")));
        assert!(!is_breaking_gap(&v("1.2.3"), &v("1.2.4")));
    }

    #[test]
    fn patch_gap_is_a_soft_warning() {
        assert_soft(&warning_for(&Version::new(0, 1, 28), &Version::new(0, 1, 29)).unwrap());
    }

    #[test]
    fn minor_gap_warns_about_breaking_changes() {
        // Pre-1.0, a bumped minor is the semver signal for a breaking change.
        assert_breaking(&warning_for(&Version::new(0, 1, 29), &Version::new(0, 2, 0)).unwrap());
    }

    #[test]
    fn major_gap_warns_about_breaking_changes() {
        assert_breaking(&warning_for(&Version::new(1, 4, 0), &Version::new(2, 0, 0)).unwrap());
    }

    #[test]
    fn post_1_0_patch_and_minor_are_not_breaking() {
        // Once past 1.0, a minor bump is additive: patch (1.2.3 -> 1.2.4) and
        // minor (1.2.3 -> 1.3.0) gaps both get the soft wording; only a major
        // bump escalates.
        assert_soft(&warning_for(&Version::new(1, 2, 3), &Version::new(1, 2, 4)).unwrap());
        assert_soft(&warning_for(&Version::new(1, 2, 3), &Version::new(1, 3, 0)).unwrap());
    }

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
