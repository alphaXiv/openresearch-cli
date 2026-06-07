//! The `artifacts` command — list the text artifacts a run produced.
//!
//! `orx artifact <runId> <key>` reads one artifact by key, but you have to know
//! the key first. This lists them (key + size), so an agent can discover what a
//! run uploaded before reading any of it.

use crate::client::list_artifacts;
use crate::error::require_credentials;
use crate::error::Result;
use crate::output::print_table;

pub async fn run(args: crate::ArtifactsArgs) -> Result<()> {
    let creds = require_credentials().await;

    let mut artifacts = list_artifacts(&creds, &args.run_id).await?.artifacts;
    if artifacts.is_empty() {
        println!("No artifacts found for this run.");
        return Ok(());
    }

    // Stable, predictable order by key.
    artifacts.sort_by(|a, b| a.key.cmp(&b.key));

    let rows: Vec<Vec<String>> = artifacts
        .iter()
        .map(|a| vec![a.key.clone(), human_bytes(a.size)])
        .collect();
    print_table(&["KEY", "SIZE"], &rows);

    eprintln!(
        "\n{} artifact(s). Read one with `orx artifact {} <key>`.",
        artifacts.len(),
        args.run_id
    );
    Ok(())
}

/// Compact human-readable byte size (e.g. `1.2 KB`, `3.4 MB`).
fn human_bytes(n: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{} B", n);
    }
    let mut size = n as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{:.1} {}", size, UNITS[unit])
}
