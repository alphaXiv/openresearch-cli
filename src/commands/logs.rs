use std::io::Write;

use crate::client::read_run_log;
use crate::error::require_credentials;
use crate::error::Result;

/// Parses a string the way JS `Number(s)` does for our purposes and returns it
/// only if it represents an integer (matching `Number.isInteger`). An empty or
/// non-numeric string yields `None` (JS produces NaN, which is not an integer).
fn parse_integer(s: &str) -> Option<i64> {
    let trimmed = s.trim();
    // JS Number("") === 0, but that branch never matters here because the inputs
    // either come from a non-empty flag value or a split that produced a piece.
    let value: f64 = trimmed.parse().ok()?;
    if value.is_finite() && value.fract() == 0.0 {
        Some(value as i64)
    } else {
        None
    }
}

/// Prints a run's terminal log. Tail by default (the end is usually what you
/// want); `--head` reads from the start, `--range <start>:<end>` an exact byte
/// window (offsets come from `orx search-logs`).
pub async fn run(args: crate::LogsArgs) -> Result<()> {
    let creds = require_credentials().await;

    let mut mode: &str = if args.head { "head" } else { "tail" };
    let mut start_byte: Option<i64> = None;
    let mut end_byte: Option<i64> = None;

    if let Some(range) = args.range.as_deref() {
        let mut parts = range.splitn(2, ':');
        let s = parts.next().unwrap_or("");
        let e = parts.next().unwrap_or("");
        let sb = parse_integer(s);
        let eb = parse_integer(e);
        match (sb, eb) {
            (Some(sb), Some(eb)) if eb > sb => {
                start_byte = Some(sb);
                end_byte = Some(eb);
            }
            _ => {
                eprintln!("--range must be <start>:<end> byte offsets with end > start.");
                std::process::exit(1);
            }
        }
        mode = "range";
    }

    let max_bytes = match args.bytes.as_deref() {
        Some(b) => match parse_integer(b) {
            Some(v) => Some(v),
            None => {
                eprintln!("--bytes must be an integer.");
                std::process::exit(1);
            }
        },
        None => None,
    };

    let log = read_run_log(
        &creds,
        &args.run_id,
        Some(mode),
        max_bytes,
        start_byte,
        end_byte,
    )
    .await?;

    // The log itself goes to stdout (pipe-friendly); metadata to stderr so it
    // doesn't pollute a `| grep` or a redirect.
    let mut stdout = std::io::stdout();
    stdout.write_all(log.content.as_bytes())?;
    if !log.content.is_empty() && !log.content.ends_with('\n') {
        stdout.write_all(b"\n")?;
    }
    stdout.flush()?;

    let span = format!(
        "bytes {}–{} of {}",
        log.start_byte, log.end_byte, log.total_bytes
    );
    let mut more: Vec<&str> = Vec::new();
    if log.truncated_before {
        more.push("more above");
    }
    if log.truncated_after {
        more.push("more below");
    }
    let more_str = if more.is_empty() {
        String::new()
    } else {
        format!(" ({})", more.join(", "))
    };
    eprintln!("[{}] {}{}", log.source, span, more_str);

    Ok(())
}
