//!
//! Render a W&B metric across runs to a PNG and save it locally. The server does
//! the rendering (W&B fetch + chart + R2); we download the result so a
//! vision-capable agent can `Read` the file to see the chart, while the printed
//! summary lets a text-only agent reason without ever opening the image.

use std::path::PathBuf;

use crate::client::{render_wandb_chart, WandbChartBody, WandbChartResult, WandbRunSpec};
use crate::error::{require_credentials, Result};

const USAGE: &str = "Usage: orx chart wandb <projectId> --metric \"<key>\" --run <runId>[:label] [--run ...] [--smoothing <0-0.99>] [--out <dir>]";

/// Where rendered PNGs land by default — a stable cache dir an agent can re-read.
fn cache_dir() -> PathBuf {
    let base = match std::env::var("XDG_CACHE_HOME") {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => {
            let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
            home.join(".cache")
        }
    };
    base.join("openresearch").join("charts")
}

/// Compact numeric formatting matching the assistant's chart summaries.
/// Mirrors JS: `0` for zero; `n.toExponential(3)` when |n|>=1e4 or |n|<1e-3;
/// otherwise `n.toPrecision(4)` (4 significant figures).
fn fmt(n: f64) -> String {
    let abs = n.abs();
    if abs == 0.0 {
        return "0".to_string();
    }
    if !(0.001..10000.0).contains(&abs) {
        return to_exponential(n, 3);
    }
    to_precision(n, 4)
}

/// JS `Number.prototype.toExponential(fractionDigits)`: mantissa with exactly
/// `digits` fraction digits, exponent with explicit sign and no leading zeros.
fn to_exponential(n: f64, digits: usize) -> String {
    // Rust's `{:e}` formatter produces e.g. "1.234e5" / "1.234e-5"; with a
    // precision it fixes the fraction digits. We then normalize to JS style
    // ("e+5" / "e-5").
    let s = format!("{:.*e}", digits, n);
    // Split mantissa and exponent.
    if let Some(pos) = s.find('e') {
        let (mantissa, exp) = s.split_at(pos);
        let exp = &exp[1..]; // strip 'e'
        let (sign, mag) = if let Some(rest) = exp.strip_prefix('-') {
            ('-', rest)
        } else if let Some(rest) = exp.strip_prefix('+') {
            ('+', rest)
        } else {
            ('+', exp)
        };
        format!("{}e{}{}", mantissa, sign, mag)
    } else {
        s
    }
}

/// JS `Number.prototype.toPrecision(precision)`: significant figures, switching
/// to exponential notation when the exponent is < -6 or >= precision.
///
/// We use Rust's correctly-rounded `{:.Ne}` formatter to round `n` to `precision`
/// significant figures and read back the *post-rounding* decimal exponent. This
/// matches JS bit-for-bit — including rounding carries across a power-of-ten
/// boundary (`9999.9` → `1.000e+4`) and half-way values whose nearest f64 tips
/// the rounding (`9.9995` → `9.999`) — rather than hand-rolling a multiply that
/// rounds differently from the float-to-decimal conversion.
fn to_precision(n: f64, precision: usize) -> String {
    if n == 0.0 {
        return "0".to_string();
    }
    let p = precision as i32;
    // Correctly-rounded sig-fig form, e.g. "9.999e0" / "1.000e4" / "-4.200e1".
    let sci = format!("{:.*e}", precision - 1, n);
    let exp: i32 = sci[sci.find('e').unwrap() + 1..].parse().unwrap_or(0);
    if exp < -6 || exp >= p {
        to_exponential(n, precision.saturating_sub(1))
    } else {
        // Fixed form with (precision - 1 - exp) fraction digits; `{:.*}` is also
        // correctly rounded, so it agrees with the exponent derived above.
        let frac = (p - 1 - exp).max(0) as usize;
        format!("{:.*}", frac, n)
    }
}

/// Parse a `--run <id>[:label]` spec into its run id and optional legend label.
fn parse_run(spec: &str) -> WandbRunSpec {
    match spec.find(':') {
        None => WandbRunSpec {
            run_id: spec.to_string(),
            label: None,
        },
        Some(idx) => {
            let label = &spec[idx + 1..];
            WandbRunSpec {
                run_id: spec[..idx].to_string(),
                label: if label.is_empty() {
                    None
                } else {
                    Some(label.to_string())
                },
            }
        }
    }
}

fn print_summary(result: &WandbChartResult) {
    println!("Metric: {}", result.metric_key);
    for s in &result.summaries {
        println!(
            "  {}: n={}, min={}, max={}, last={}",
            s.label,
            s.n,
            fmt(s.min),
            fmt(s.max),
            fmt(s.last)
        );
    }
    if !result.failed.is_empty() {
        println!("Skipped:");
        for f in &result.failed {
            println!("  {}: {}", f.label, f.error);
        }
    }
}

/// Build the filename slug from the metric key: non-alphanumeric runs collapse
/// to `-`, leading/trailing `-` trimmed, lowercased; fallback `"metric"`.
fn slugify(metric_key: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in metric_key.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "metric".to_string()
    } else {
        trimmed.to_string()
    }
}

pub async fn run(args: crate::ChartArgs) -> Result<()> {
    if args.kind != "wandb" || args.metric.is_none() || args.run.is_empty() {
        eprintln!("{USAGE}");
        std::process::exit(1);
    }

    let metric = args.metric.as_deref().unwrap();

    let mut smoothing: Option<f64> = None;
    if let Some(raw) = args.smoothing.as_deref() {
        match raw.trim().parse::<f64>() {
            Ok(v) if !v.is_nan() && (0.0..=0.99).contains(&v) => smoothing = Some(v),
            _ => {
                eprintln!("--smoothing must be a number between 0 and 0.99");
                std::process::exit(1);
            }
        }
    }

    let creds = require_credentials().await;
    let body = WandbChartBody {
        metric_key: metric.to_string(),
        runs: args.run.iter().map(|s| parse_run(s)).collect(),
        smoothing,
    };
    let result = render_wandb_chart(&creds, &args.project_id, &body).await?;

    print_summary(&result);

    let (url, chart_id) = match (&result.url, &result.chart_id) {
        (Some(url), Some(chart_id)) if !url.is_empty() && !chart_id.is_empty() => {
            (url.clone(), chart_id.clone())
        }
        _ => {
            eprintln!(
                "\nNo chart rendered for '{}' — see skipped runs above.",
                result.metric_key
            );
            std::process::exit(1);
        }
    };

    // Download the PNG so the agent can Read it from disk. Plain GET — the URL
    // is presigned and carries no auth header.
    let res = reqwest::get(&url).await?;
    if !res.status().is_success() {
        let status = res.status();
        eprintln!(
            "\nFailed to download chart image ({} {}).",
            status.as_u16(),
            status.canonical_reason().unwrap_or("")
        );
        std::process::exit(1);
    }
    let bytes = res.bytes().await?;

    let dir = match &args.out {
        Some(out) => PathBuf::from(out),
        None => cache_dir(),
    };
    tokio::fs::create_dir_all(&dir).await?;

    let slug = slugify(&result.metric_key);
    let chart_prefix: String = chart_id.chars().take(8).collect();
    let file = dir.join(format!("{slug}-{chart_prefix}.png"));
    tokio::fs::write(&file, &bytes).await?;

    println!("\nChart: {}", file.display());
    println!("Read this PNG file to view the chart.");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::fmt;

    // Expected values are JS `fmt()` outputs (Number.toPrecision(4) /
    // toExponential(3)), including the carry cases
    // that the naive port got wrong.
    #[test]
    fn fmt_matches_js() {
        assert_eq!(fmt(0.0), "0");
        // carry across a power-of-ten boundary -> exponential
        assert_eq!(fmt(9999.9), "1.000e+4");
        assert_eq!(fmt(9999.6), "1.000e+4");
        // carry that stays in fixed form
        assert_eq!(fmt(999.95), "1000");
        assert_eq!(fmt(9.9999), "10.00");
        assert_eq!(fmt(99.999), "100.0");
        // non-carry (float-faithful with JS)
        assert_eq!(fmt(9.9995), "9.999");
        // exact power of ten (log10 floor pitfall)
        assert_eq!(fmt(1000.0), "1000");
        assert_eq!(fmt(100.0), "100.0");
        // pure exponential path (|n| >= 1e4 or < 1e-3)
        assert_eq!(fmt(99999.0), "1.000e+5");
        assert_eq!(fmt(0.0001234), "1.234e-4");
        // ordinary values
        assert_eq!(fmt(3.21459), "3.215");
        assert_eq!(fmt(-42.0), "-42.00");
    }
}
