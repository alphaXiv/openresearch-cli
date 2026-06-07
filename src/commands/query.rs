//! Runs one read-only DuckDB SQL statement against a project's evidence schema.

use crate::client::{query_project, SyncStatus};
use crate::error::{require_credentials, Result};
use crate::output::{cell, print_table};

/// Renders the wire form of a sync status (matches the TS `"ready" | ...` union).
fn sync_status_str(status: SyncStatus) -> &'static str {
    match status {
        SyncStatus::Degraded => "degraded",
        SyncStatus::Ready => "ready",
        SyncStatus::Warming => "warming",
    }
}

pub async fn run(args: crate::QueryArgs) -> Result<()> {
    // clap enforces the required positionals, so the TS `if (!projectId || !sql)`
    // usage guard is unnecessary here.
    let creds = require_credentials().await;
    let result = query_project(&creds, &args.project_id, &args.sql).await?;

    // The evidence cache warms asynchronously; tell the user when it isn't ready
    // so empty/partial results aren't mistaken for "no data".
    if result.sync_status != SyncStatus::Ready {
        eprintln!(
            "[sync: {}] results may be incomplete",
            sync_status_str(result.sync_status)
        );
        for e in &result.sync_errors {
            eprintln!("  {}", e);
        }
    }

    if result.columns.is_empty() {
        println!("(no columns returned)");
        return Ok(());
    }

    let headers: Vec<&str> = result.columns.iter().map(|c| c.as_str()).collect();
    let rows: Vec<Vec<String>> = result
        .rows
        .iter()
        .map(|row| row.iter().map(cell).collect())
        .collect();
    print_table(&headers, &rows);

    if result.more_rows_available {
        eprintln!(
            "\nShowing {} of {} rows (add LIMIT/OFFSET to page).",
            result.row_count, result.total_row_count
        );
    }

    Ok(())
}
