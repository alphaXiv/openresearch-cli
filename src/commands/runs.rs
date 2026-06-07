//! Lists a project's runs as a table, newest first.

use std::collections::HashMap;

use crate::client::{list_experiments, list_runs};
use crate::error::{require_credentials, Result};
use crate::output::print_table;

/// Lists a project's runs as a table, newest first.
pub async fn run(args: crate::RunsArgs) -> Result<()> {
    let creds = require_credentials().await;

    // Fetch experiments too, so we can label each run with its experiment title
    // rather than a bare id. Both requests run concurrently (TS Promise.all).
    let (runs_res, experiments_res) = tokio::join!(
        list_runs(&creds, &args.project_id),
        list_experiments(&creds, &args.project_id)
    );
    let runs = runs_res?.runs;
    let experiments = experiments_res?.experiments;

    let title_of: HashMap<String, String> =
        experiments.into_iter().map(|e| (e.id, e.title)).collect();

    let mut filtered: Vec<_> = match &args.experiment {
        Some(exp_id) => runs
            .into_iter()
            .filter(|r| &r.experiment_id == exp_id)
            .collect(),
        None => runs,
    };

    // Run ids are UUIDv7 — lexicographic sort is chronological. Newest first.
    filtered.sort_by(|a, b| b.id.cmp(&a.id));

    if filtered.is_empty() {
        println!("No runs found.");
        return Ok(());
    }

    let rows: Vec<Vec<String>> = filtered
        .into_iter()
        .map(|r| {
            vec![
                r.status,
                title_of
                    .get(&r.experiment_id)
                    .cloned()
                    .unwrap_or(r.experiment_id),
                match &r.commit_sha {
                    Some(sha) => sha.chars().take(7).collect::<String>(),
                    None => "—".to_string(),
                },
                r.updated_at,
            ]
        })
        .collect();

    print_table(&["STATUS", "EXPERIMENT", "COMMIT", "UPDATED"], &rows);

    Ok(())
}
