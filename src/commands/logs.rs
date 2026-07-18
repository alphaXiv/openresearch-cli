use std::io::{Read as _, Seek as _, Write};

use crate::client::read_run_log;
use crate::error::require_credentials;
use crate::error::Result;
use crate::local::resolve::{resolve_run, RunRef};
use crate::store::{log_path, Store};

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

    // Local run (orx up): the log is a plain file beside the store — read it
    // directly, no api / login needed.
    let store = Store::open()?;
    match resolve_run(&store, &args.run_id)? {
        RunRef::Local(_) => run_local(&args.run_id, mode, max_bytes, start_byte, end_byte),
        RunRef::Server(_) => run_server(&args.run_id, mode, max_bytes, start_byte, end_byte).await,
    }
}

/// Server-mode log read via the api.
async fn run_server(
    run_id: &str,
    mode: &str,
    max_bytes: Option<i64>,
    start_byte: Option<i64>,
    end_byte: Option<i64>,
) -> Result<()> {
    let creds = require_credentials().await;

    let log = read_run_log(&creds, run_id, Some(mode), max_bytes, start_byte, end_byte).await?;

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

/// Default byte window for local head/tail reads without `--bytes`.
const LOCAL_DEFAULT_BYTES: i64 = 64 * 1024;

/// Local-mode log read: same head/tail/range semantics over the run's
/// `run-logs/<id>.log` file, same stdout/stderr split as the server path.
fn run_local(
    run_id: &str,
    mode: &str,
    max_bytes: Option<i64>,
    start_byte: Option<i64>,
    end_byte: Option<i64>,
) -> Result<()> {
    let path = log_path(run_id);
    let total = match std::fs::metadata(&path) {
        Ok(m) => m.len() as i64,
        Err(_) => {
            eprintln!("[local file] no log captured yet for this run.");
            return Ok(());
        }
    };

    let max = max_bytes.unwrap_or(LOCAL_DEFAULT_BYTES).max(0);
    let (start, end) = match mode {
        "range" => (
            start_byte.unwrap_or(0).clamp(0, total),
            end_byte.unwrap_or(total).clamp(0, total),
        ),
        "head" => (0, max.min(total)),
        _ => ((total - max).max(0), total),
    };

    let mut content = Vec::new();
    if end > start {
        let mut f = std::fs::File::open(&path)?;
        f.seek(std::io::SeekFrom::Start(start as u64))?;
        f.take((end - start) as u64).read_to_end(&mut content)?;
    }

    let mut stdout = std::io::stdout();
    stdout.write_all(&content)?;
    if !content.is_empty() && !content.ends_with(b"\n") {
        stdout.write_all(b"\n")?;
    }
    stdout.flush()?;

    let mut more: Vec<&str> = Vec::new();
    if start > 0 {
        more.push("more above");
    }
    if end < total {
        more.push("more below");
    }
    let more_str = if more.is_empty() {
        String::new()
    } else {
        format!(" ({})", more.join(", "))
    };
    eprintln!(
        "[local file] bytes {}–{} of {}{}",
        start, end, total, more_str
    );

    Ok(())
}
