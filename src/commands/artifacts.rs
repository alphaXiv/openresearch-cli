//! The `artifacts` command — list the text artifacts a run produced.
//!
//! `orx artifact <runId> <key>` reads one artifact by key, but you have to know
//! the key first. This lists them (key + size), so an agent can discover what a
//! run uploaded before reading any of it.

use crate::client::list_artifacts;
use crate::error::require_credentials;
use crate::error::Result;
use crate::local::resolve::resolve_run;
use crate::output::print_table;

pub async fn run(args: crate::ArtifactsArgs) -> Result<()> {
    let store = crate::store::Store::open()?;
    if resolve_run(&store, &args.run_id)?.is_local() {
        return Err(crate::local::unsupported("artifacts"));
    }
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
        .map(|a| {
            vec![
                a.key.clone(),
                crate::store::human_bytes(a.size.max(0) as u64),
            ]
        })
        .collect();
    print_table(&["KEY", "SIZE"], &rows);

    eprintln!(
        "\n{} artifact(s). Read one with `orx artifact {} <key>`.",
        artifacts.len(),
        args.run_id
    );
    Ok(())
}
