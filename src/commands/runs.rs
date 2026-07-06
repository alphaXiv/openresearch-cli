//! Lists a project's runs as a table, newest first.

use std::collections::HashMap;

use crate::client::{list_experiments, list_runs};
use crate::error::{require_credentials, Result};
use crate::output::{format_duration, print_table, run_failure_detail};
use crate::store::Store;

/// Lists a project's runs as a table, newest first.
pub async fn run(args: crate::RunsArgs) -> Result<()> {
    // Local project (orx up): the store is the truth, no api / login needed.
    let store = Store::open()?;
    if store.get_local_project(&args.project_id)?.is_some() {
        return run_local(&store, &args);
    }

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

    // Collect why each failed run failed, to print under the table — the reason
    // (provider error on spin-up failures) doesn't fit a fixed-width column.
    let failures: Vec<(String, String)> = filtered
        .iter()
        .filter_map(|r| run_failure_detail(r).map(|d| (r.id.clone(), d)))
        .collect();

    let rows: Vec<Vec<String>> = filtered
        .into_iter()
        .map(|r| {
            vec![
                r.id,
                r.status,
                title_of
                    .get(&r.experiment_id)
                    .cloned()
                    .unwrap_or(r.experiment_id),
                match &r.commit_sha {
                    Some(sha) => sha.chars().take(7).collect::<String>(),
                    None => "—".to_string(),
                },
                format_duration(r.duration_seconds),
                r.updated_at,
            ]
        })
        .collect();

    print_table(
        &[
            "ID",
            "STATUS",
            "EXPERIMENT",
            "COMMIT",
            "DURATION",
            "UPDATED",
        ],
        &rows,
    );

    if !failures.is_empty() {
        println!();
        for (id, detail) in &failures {
            println!("{id}  {detail}");
        }
    }

    Ok(())
}

/// Local-mode listing from the store. Same table shape as the server path;
/// timestamps render as relative ("3m ago") since the store keeps unix millis.
fn run_local(store: &Store, args: &crate::RunsArgs) -> Result<()> {
    let title_of: HashMap<String, String> = store
        .list_experiments_by_project(&args.project_id)?
        .into_iter()
        .map(|e| (e.id.clone(), e.display_name().to_string()))
        .collect();

    // Already newest-first (store orders by created_at DESC).
    let runs = store.list_runs_by_project(&args.project_id)?;
    let filtered: Vec<_> = match &args.experiment {
        Some(exp_id) => runs
            .into_iter()
            .filter(|r| &r.experiment_id == exp_id)
            .collect(),
        None => runs,
    };

    if filtered.is_empty() {
        println!("No runs found.");
        return Ok(());
    }

    let failures: Vec<(String, String)> = filtered
        .iter()
        .filter_map(|r| crate::local::run_failure_detail(r).map(|d| (r.id.clone(), d)))
        .collect();

    let rows: Vec<Vec<String>> = filtered
        .into_iter()
        .map(|r| {
            vec![
                r.id.clone(),
                r.status.clone(),
                title_of
                    .get(&r.experiment_id)
                    .cloned()
                    .unwrap_or_else(|| r.experiment_id.clone()),
                match &r.commit_sha {
                    Some(sha) => sha.chars().take(7).collect::<String>(),
                    None => "—".to_string(),
                },
                format_duration(crate::local::run_duration_secs(&r)),
                crate::local::fmt_ago(r.updated_at),
            ]
        })
        .collect();

    print_table(
        &[
            "ID",
            "STATUS",
            "EXPERIMENT",
            "COMMIT",
            "DURATION",
            "UPDATED",
        ],
        &rows,
    );

    if !failures.is_empty() {
        println!();
        for (id, detail) in &failures {
            println!("{id}  {detail}");
        }
    }

    Ok(())
}
