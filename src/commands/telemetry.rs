//! `orx telemetry status | on | off` — inspect and control anonymous usage
//! analytics. The discoverable, persistent opt-out that the post-login notice
//! points at.

use crate::error::{anyhow, Result};
use crate::telemetry;

pub async fn run(args: crate::TelemetryArgs) -> Result<()> {
    match args.command {
        crate::TelemetryCommand::Status => status(),
        crate::TelemetryCommand::On => set_disabled(false),
        crate::TelemetryCommand::Off => set_disabled(true),
    }
}

fn status() -> Result<()> {
    // `--no-telemetry` is a per-run flag, not persisted state, so status reports
    // the standing configuration (env + persisted setting) with flag=false.
    match telemetry::disabled_reason(false) {
        None => {
            println!("Anonymous usage analytics: on");
            println!("  No code, prompts, file contents, or identifiers are ever sent.");
        }
        Some(reason) => {
            println!("Anonymous usage analytics: off ({})", reason.as_str());
        }
    }

    match telemetry::load_settings().and_then(|s| s.install_id) {
        Some(id) => println!("  Anonymous install id: {id}"),
        None => println!("  Anonymous install id: (not yet generated)"),
    }
    println!();
    println!("Turn off with `orx telemetry off`; back on with `orx telemetry on`.");
    Ok(())
}

fn set_disabled(disabled: bool) -> Result<()> {
    telemetry::set_persisted_disabled(disabled)
        .map_err(|e| anyhow!("Could not save telemetry setting: {e}"))?;
    if disabled {
        println!("\u{2713} Anonymous usage analytics disabled on this machine.");
    } else {
        println!("\u{2713} Anonymous usage analytics enabled.");
        println!("  (The --no-telemetry flag still disables it for a single run.)");
    }
    Ok(())
}
