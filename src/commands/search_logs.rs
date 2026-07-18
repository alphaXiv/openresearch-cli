//!
//! Greps a project's run logs for a literal pattern. Scope to one run with
//! `--run`, or one experiment's runs with `--experiment` (one is required).

use crate::client::{search_logs, SearchLogsBody};
use crate::error::{require_credentials, Result};
use crate::local::resolve::resolve_project;

pub async fn run(args: crate::SearchLogsArgs) -> Result<()> {
    if args.run.is_none() && args.experiment.is_none() {
        eprintln!("Provide --run <id> or --experiment <id> to scope the search.");
        std::process::exit(1);
    }

    let store = crate::store::Store::open()?;
    if resolve_project(&store, &args.project_id)?.is_local() {
        return Err(crate::local::unsupported("search-logs"));
    }
    let creds = require_credentials().await;

    let max_matching_lines = args.max.as_deref().map(parse_number);

    let body = SearchLogsBody {
        pattern: args.pattern,
        run_id: args.run,
        experiment_id: args.experiment,
        max_matching_lines,
    };

    let result = search_logs(&creds, &args.project_id, &body).await?;

    let mut total: u64 = 0;
    for run in &result.results {
        if run.matching_lines.is_empty() {
            continue;
        }
        // grep-style: short run id, line number, then the matching line. Byte
        // offsets are tucked at the end for feeding back into `orx logs --range`.
        let short: String = run.run_id.chars().take(8).collect();
        for m in &run.matching_lines {
            println!(
                "{}:{}: {}  \u{2190} {}:{}",
                short, m.line_number, m.text, m.start_byte, m.end_byte
            );
            total += 1;
        }
    }

    if total == 0 {
        eprintln!("No matches.");
        return Ok(());
    }

    let capped_note = if result.capped {
        " (capped — narrow the search or raise --max)"
    } else {
        ""
    };
    eprintln!("\n{} matching line(s){}.", total, capped_note);

    Ok(())
}

/// Mirror JS `Number(s)`: a non-numeric string yields NaN there, which would
/// serialize to `null`. The body field is `Option<i64>`, so we coerce a failed
/// parse to a sentinel that matches JS truncation toward zero for plain ints.
fn parse_number(s: &str) -> i64 {
    let t = s.trim();
    if let Ok(n) = t.parse::<i64>() {
        return n;
    }
    // Fall back to float parsing (JS Number accepts "10.0", "1e3", etc.),
    // truncating toward zero like a typical integer coercion downstream.
    t.parse::<f64>().map(|f| f as i64).unwrap_or(0)
}
