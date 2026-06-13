//! Display helpers for tabular and cell output.

use serde_json::Value;

/// Formats a duration in seconds as a compact human string: `1h42m`, `3m12s`,
/// `45s`. Negative inputs render as an em dash.
pub fn format_duration(secs: i64) -> String {
    if secs < 0 {
        return "—".to_string();
    }
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Renders rows as a left-aligned, space-padded table with a header row and a
/// box-drawing separator line,.
///
/// Column widths are the max of the header and every cell. Cells are joined
/// with two spaces, and trailing whitespace is trimmed per line.
pub fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let cols = headers.len();
    let mut widths = vec![0usize; cols];
    for (i, h) in headers.iter().enumerate() {
        widths[i] = h.chars().count();
    }
    for row in rows {
        for (i, w) in widths.iter_mut().enumerate() {
            let len = row.get(i).map(|c| c.chars().count()).unwrap_or(0);
            if len > *w {
                *w = len;
            }
        }
    }

    let line = |cells: &[String]| -> String {
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| pad_end(c, widths.get(i).copied().unwrap_or(0)))
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string()
    };

    let header_cells: Vec<String> = headers.iter().map(|h| h.to_string()).collect();
    println!("{}", line(&header_cells));

    let sep: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
    println!("{}", line(&sep));

    for row in rows {
        println!("{}", line(row));
    }
}

/// Pads `s` on the right with spaces to `width` (measured in chars).
fn pad_end(s: &str, width: usize) -> String {
    let len = s.chars().count();
    if len >= width {
        s.to_string()
    } else {
        format!("{}{}", s, " ".repeat(width - len))
    }
}

/// Formats an arbitrary SQL cell value for display, matching the TS `cell()`:
/// `null`/absent -> "", objects/arrays -> compact JSON, scalars -> their string.
pub fn cell(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        // Objects and arrays: compact JSON, like `JSON.stringify`.
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}
