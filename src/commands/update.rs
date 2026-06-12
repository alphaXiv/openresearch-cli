//! The `update` command — update orx in place by re-running the release
//! installer.
//!
//! The mechanism mirrors what axoupdater (uv's `self update`) does: download
//! the `openresearch-cli-installer.sh` asset from the target release and run
//! it pinned to the existing install prefix via `CARGO_DIST_FORCE_INSTALL_DIR`.
//! The installer owns the hard parts — checksum verification and the atomic
//! rename into `~/.cargo/bin` (never an in-place overwrite, which on macOS
//! trips the kernel's per-inode code-signature cache and SIGKILLs the binary).
//!
//! Guards, in order:
//!   - `OPENRESEARCH_CLI_DISABLE_UPDATE=1` refuses outright (same switch the
//!     installer honors).
//!   - an exclusive file lock, so concurrent `orx update` runs fail fast
//!     instead of corrupting each other.
//!   - no install receipt -> this binary wasn't installed by the installer
//!     script (most likely `cargo install`); refuse with the right
//!     alternative, since both paths land in `~/.cargo/bin/orx`.
//!   - receipt prefix or version not matching the running binary -> another
//!     copy exists or something else overwrote it; require `--force`.
//!   - Nix-store / Homebrew paths and unwritable bin dirs refuse with
//!     targeted messages.

use std::path::PathBuf;
use std::time::Duration;

use crate::error::{anyhow, Result};
use crate::updates;

const INSTALL_HINT: &str = "curl --proto '=https' --tlsv1.2 -LsSf \
https://github.com/alphaXiv/openresearch-cli/releases/latest/download/openresearch-cli-installer.sh | sh";

pub async fn run(args: crate::UpdateArgs) -> Result<()> {
    if std::env::var("OPENRESEARCH_CLI_DISABLE_UPDATE").as_deref() == Ok("1") {
        return Err(anyhow!(
            "Updates are disabled for this install (OPENRESEARCH_CLI_DISABLE_UPDATE=1)."
        ));
    }

    // One updater at a time. The lock file lives in our config dir; flock is
    // advisory and released automatically when the process exits.
    let lock_path = crate::config::config_dir().join("update.lock");
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)?;
    let mut lock = fd_lock::RwLock::new(lock_file);
    let _guard = lock
        .try_write()
        .map_err(|_| anyhow!("Another `orx update` is already running."))?;

    let current = updates::current_version();
    let exe = std::env::current_exe()?
        .canonicalize()
        .map_err(|e| anyhow!("Could not resolve the running executable: {}", e))?;

    // Package-manager-shaped paths get a targeted refusal before any network.
    let exe_str = exe.to_string_lossy();
    if exe_str.starts_with("/nix/store/") {
        return Err(anyhow!(
            "This orx is managed by Nix ({}). Update it through your Nix configuration.",
            exe.display()
        ));
    }
    if exe_str.starts_with("/opt/homebrew/") || exe_str.contains("/Cellar/") {
        return Err(anyhow!(
            "This orx looks Homebrew-managed ({}). Update it with `brew upgrade`.",
            exe.display()
        ));
    }

    // The receipt is the only discriminator between an installer-managed
    // binary and a `cargo install` one — both live at ~/.cargo/bin/orx.
    let Some(receipt) = updates::load_receipt()? else {
        return Err(anyhow!(
            "orx was not installed by the installer script (no receipt at {}),\n\
             so `orx update` won't touch it. Update it the way it was installed:\n\
             - cargo: cargo install --path . (or your original cargo install invocation)\n\
             - or reinstall with the installer: {}",
            updates::receipt_path().display(),
            INSTALL_HINT
        ));
    };

    let prefix = PathBuf::from(&receipt.install_prefix);
    let prefix = prefix.canonicalize().unwrap_or(prefix);
    if !updates::exe_matches_prefix(&exe, &prefix) && !args.force {
        return Err(anyhow!(
            "The running orx is at {} but the installer's receipt says it installed to {}.\n\
             Are multiple copies of orx installed? Pass --force to update the receipt's copy anyway.",
            exe.display(),
            prefix.display()
        ));
    }
    if receipt.version != current.to_string() && !args.force {
        return Err(anyhow!(
            "The running orx is {} but the install receipt records {} — something other than\n\
             the installer (likely `cargo install`) overwrote {}. Updating would clobber it.\n\
             Pass --force to proceed anyway.",
            current,
            receipt.version,
            exe.display()
        ));
    }

    // Probe writability of the bin dir up front (root-owned installs,
    // read-only filesystems) so we fail before downloading anything.
    if let Some(bin_dir) = exe.parent() {
        let probe = bin_dir.join(format!(".orx-update-probe-{}", uuid::Uuid::new_v4()));
        match std::fs::File::create(&probe) {
            Ok(_) => {
                let _ = std::fs::remove_file(&probe);
            }
            Err(e) => {
                return Err(anyhow!(
                    "No write permission for {} ({}). If orx was installed with sudo,\n\
                     update it the same way or reinstall per-user.",
                    bin_dir.display(),
                    e
                ));
            }
        }
    }

    let latest = updates::fetch_latest(Duration::from_secs(10)).await?;
    if latest.version <= current {
        println!("orx {} is up to date.", current);
        return Ok(());
    }

    if args.dry_run {
        println!(
            "orx {} → {} is available. Re-run without --dry-run to update.",
            current, latest.version
        );
        return Ok(());
    }

    eprintln!("Updating orx {} → {} ...", current, latest.version);

    // Pin the installer to the same release the manifest described, so the
    // version we report is exactly the version that gets installed.
    let installer = updates::fetch_release_asset(
        &latest.tag,
        &format!("{}-installer.sh", updates::APP_NAME),
        Duration::from_secs(60),
    )
    .await?;
    let script = std::env::temp_dir().join(format!("orx-installer-{}.sh", uuid::Uuid::new_v4()));
    std::fs::write(&script, &installer)?;

    // `sh <script>` rather than executing the file: immune to noexec /tmp
    // mounts. The installer verifies artifact checksums and renames the new
    // binary into place atomically; replacing a running orx is safe on
    // macOS/Linux (old processes keep the old inode).
    let mut cmd = std::process::Command::new("sh");
    cmd.arg(&script)
        .env("CARGO_DIST_FORCE_INSTALL_DIR", &receipt.install_prefix);
    if !receipt.modify_path {
        cmd.env("OPENRESEARCH_CLI_NO_MODIFY_PATH", "1");
    }
    let status = cmd.status();
    let _ = std::fs::remove_file(&script);
    let status = status.map_err(|e| anyhow!("Could not run the installer: {}", e))?;
    if !status.success() {
        return Err(anyhow!(
            "The installer exited with {}. The previous orx is untouched.",
            status
        ));
    }

    // Keep the nudge cache in sync so it doesn't fire on a stale answer.
    updates::write_check_cache(&latest.version.to_string());
    println!("✓ Updated orx {} → {}.", current, latest.version);

    // The refreshed SKILL.md ships inside the *new* binary, which this (old)
    // process can't read — so nudge instead of silently leaving stale copies.
    if let Some(home) = dirs::home_dir() {
        if [".claude/skills", ".agents/skills"]
            .iter()
            .any(|rel| home.join(rel).join("openresearch-cli").exists())
        {
            println!("Installed orx skills may be stale — run: orx skill install --all");
        }
    }
    Ok(())
}
