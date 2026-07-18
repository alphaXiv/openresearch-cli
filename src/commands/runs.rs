//! Lists a project's runs as a table, newest first.

use crate::error::Result;
use crate::output::{format_duration, print_table};
use crate::plane::resolve_project;
use crate::store::Store;

/// Lists a project's runs as a table, newest first.
pub async fn run(args: crate::RunsArgs) -> Result<()> {
    // Local project (orx up): the store is the truth, no api / login needed —
    // the plane resolver decides which side owns the id.
    let store = Store::open()?;
    let plane = resolve_project(store, &args.project_id)?;
    let listing = plane.list_runs().await?;

    let filtered: Vec<_> = match &args.experiment {
        Some(exp_id) => listing
            .runs
            .into_iter()
            .filter(|r| &r.experiment_id == exp_id)
            .collect(),
        None => listing.runs,
    };

    if filtered.is_empty() {
        println!("No runs found.");
        return Ok(());
    }

    // Collect why each failed run failed, to print under the table — the reason
    // (provider error on spin-up failures) doesn't fit a fixed-width column.
    let failures: Vec<(String, String)> = filtered
        .iter()
        .filter_map(|r| r.failure_detail().map(|d| (r.id.clone(), d)))
        .collect();

    let rows: Vec<Vec<String>> = filtered
        .iter()
        .map(|r| {
            vec![
                r.id.clone(),
                r.status.clone(),
                listing
                    .titles
                    .get(&r.experiment_id)
                    .cloned()
                    .unwrap_or_else(|| r.experiment_id.clone()),
                match &r.commit_sha {
                    Some(sha) => sha.chars().take(7).collect::<String>(),
                    None => "—".to_string(),
                },
                format_duration(r.duration_secs),
                r.updated_display.clone(),
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
