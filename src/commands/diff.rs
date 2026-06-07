//! The `diff` command — print a run's cumulative code diff.
//!
//! Shows the unified diff between a run's commit and its parent experiment's
//! branch (i.e. what this run changed). Diff to stdout (pipe-friendly), metadata
//! to stderr.

use std::io::Write;

use crate::client::get_run_diff;
use crate::error::require_credentials;
use crate::error::Result;

pub async fn run(args: crate::DiffArgs) -> Result<()> {
    let creds = require_credentials().await;

    let d = get_run_diff(&creds, &args.run_id).await?;

    if d.diff.is_empty() {
        eprintln!("No diff — this run's commit matches its parent branch.");
        return Ok(());
    }

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    out.write_all(d.diff.as_bytes())?;
    if !d.diff.ends_with('\n') {
        out.write_all(b"\n")?;
    }
    out.flush()?;

    if d.truncated {
        eprintln!(
            "[diff truncated: {} of {} byte limit reached]",
            d.bytes_read, d.byte_limit
        );
    }
    Ok(())
}
