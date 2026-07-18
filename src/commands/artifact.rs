//! The `artifact` command.
//!
//! Prints a bounded excerpt of a run's text artifact. Tail by default; `--head`
//! reads from the start. Content goes to stdout (pipe-friendly), metadata to
//! stderr.

use std::io::Write;

use crate::client::read_artifact;
use crate::error::require_credentials;
use crate::error::{anyhow, Result};
use crate::local::resolve::resolve_run;

pub async fn run(args: crate::ArtifactArgs) -> Result<()> {
    let store = crate::store::Store::open()?;
    if resolve_run(&store, &args.run_id)?.is_local() {
        return Err(crate::local::unsupported("artifact"));
    }
    let creds = require_credentials().await;

    let mode = if args.head { "head" } else { "tail" };

    let max_bytes: Option<i64> = match args.bytes.as_deref() {
        Some(s) => {
            // Match TS: Number(s) parsed, must be an integer.
            let n: f64 = s
                .trim()
                .parse::<f64>()
                .map_err(|_| anyhow!("--bytes must be an integer."))?;
            if n.fract() != 0.0 || !n.is_finite() {
                return Err(anyhow!("--bytes must be an integer."));
            }
            Some(n as i64)
        }
        None => None,
    };

    let a = read_artifact(&creds, &args.run_id, &args.key, Some(mode), max_bytes).await?;

    // Content to stdout (pipe-friendly); metadata to stderr.
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    out.write_all(a.content.as_bytes())?;
    if !a.content.is_empty() && !a.content.ends_with('\n') {
        out.write_all(b"\n")?;
    }
    out.flush()?;

    let span = format!("bytes {}–{} of {}", a.start_byte, a.end_byte, a.total_bytes);
    let mut more: Vec<&str> = Vec::new();
    if a.truncated_before {
        more.push("more above");
    }
    if a.truncated_after {
        more.push("more below");
    }
    let suffix = if more.is_empty() {
        String::new()
    } else {
        format!(" ({})", more.join(", "))
    };
    eprintln!("[{}] {}{}", a.key, span, suffix);

    Ok(())
}
