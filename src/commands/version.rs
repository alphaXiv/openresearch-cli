//! The `version` command — print the CLI version, optionally comparing it to
//! the latest GitHub release.
//!
//! `--json` (which implies `--check`) is the agent-facing form: one stable
//! JSON object on stdout, exit 0 whether or not an update is available, so
//! harnesses can poll deliberately instead of scraping stderr nudges.

use std::time::Duration;

use crate::error::Result;
use crate::updates;

pub async fn run(args: crate::VersionArgs) -> Result<()> {
    let current = updates::current_version();

    if !args.check && !args.json {
        println!("orx {}", current);
        return Ok(());
    }

    let latest = updates::fetch_latest(Duration::from_secs(10)).await?;
    // Keep the passive nudge's cache in sync with this explicit check.
    updates::write_check_cache(&latest.version.to_string());
    let update_available = latest.version > current;

    if args.json {
        println!(
            "{}",
            serde_json::json!({
                "current": current.to_string(),
                "latest": latest.version.to_string(),
                "updateAvailable": update_available,
            })
        );
        return Ok(());
    }

    println!("orx {}", current);
    if update_available {
        println!(
            "A new release is available: {} → {}. Run `orx update` to upgrade.",
            current, latest.version
        );
    } else {
        println!("orx is up to date.");
    }
    Ok(())
}
