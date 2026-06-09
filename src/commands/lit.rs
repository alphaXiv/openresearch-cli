//! The `lit` command — full-text literature search over alphaXiv.
//!
//! Public endpoint, no token required. Prints a compact, agent-readable list of
//! hits (id, title, date, votes, truncated abstract) by default, or raw JSON with
//! `--json`. Pull a hit's report next with `orx paper <id>`.

use crate::client::search_papers;
use crate::error::Result;

pub async fn run(args: crate::LitArgs) -> Result<()> {
    let limit = args.limit.unwrap_or(5);
    let hits = search_papers(&args.query, limit).await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
        return Ok(());
    }

    if hits.is_empty() {
        eprintln!("No papers found for {:?}.", args.query);
        return Ok(());
    }

    for h in &hits {
        let date = h
            .publication_date
            .as_deref()
            .and_then(|d| d.split('T').next())
            .unwrap_or("—");
        println!("{}  {}", h.paper_id, h.title);
        println!("            {} · {} votes", date, h.votes);
        let abstract_ = collapse_ws(&h.abstract_);
        if !abstract_.is_empty() {
            println!("            {}", truncate_chars(&abstract_, 300));
        }
        println!();
    }
    eprintln!("Fetch a report with: orx paper <paperId>");
    Ok(())
}

/// Collapse runs of whitespace (incl. newlines) into single spaces and trim.
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncate to at most `max` chars, appending `…` when shortened.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}
