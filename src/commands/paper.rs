//! The `paper` command — fetch a paper's machine-readable report (default) or
//! its full extracted text (`--full`) from alphaXiv.
//!
//! Public endpoint, no token required. The report is the structured, LLM-oriented
//! analysis (≈10 KB, usually enough); `--full` is the raw extracted text and is a
//! fallback when the report lacks a specific equation/table/section. Markdown goes
//! to stdout (pipe/redirect-friendly). If alphaXiv has a GitHub repo linked to the
//! paper, a `GitHub: <url>` line is printed first regardless of flags.

use crate::client::{fetch_paper_github, fetch_paper_markdown};
use crate::error::{anyhow, Result};

pub async fn run(args: crate::PaperArgs) -> Result<()> {
    let id = parse_paper_id(&args.id);
    let kind = if args.full { "abs" } else { "overview" };

    let (md, github) = tokio::join!(fetch_paper_markdown(kind, &id), fetch_paper_github(&id));

    // Best-effort: the GitHub link is useful context, never a reason to fail.
    if let Ok(Some(url)) = github {
        println!("GitHub: {}", url);
        println!();
    }

    match md? {
        Some(md) => {
            println!("{}", md);
            Ok(())
        }
        None if args.full => Err(anyhow!(
            "No full text extracted for {id} yet. Last resort — the PDF: https://arxiv.org/pdf/{id}"
        )),
        None => Err(anyhow!(
            "No report generated for {id} yet. Try `orx paper {id} --full` for the raw extracted text."
        )),
    }
}

/// Normalize whatever the user passes (bare id, versioned id, or an arXiv /
/// alphaXiv URL) into a canonical paper id like `2401.12345` or `2401.12345v2`.
///
/// Handles `arxiv.org/abs/<id>`, `arxiv.org/pdf/<id>[.pdf]`,
/// `alphaxiv.org/overview/<id>`, `alphaxiv.org/abs/<id>`, and bare ids — by
/// taking the last path segment and stripping any `?`/`#` and `.pdf`/`.md` suffix.
pub(crate) fn parse_paper_id(input: &str) -> String {
    let s = input.trim();
    let s = s.split(['?', '#']).next().unwrap_or(s);
    let last = s.rsplit('/').next().unwrap_or(s);
    last.trim_end_matches(".pdf")
        .trim_end_matches(".md")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::parse_paper_id;

    #[test]
    fn parses_all_forms() {
        let cases = [
            ("2401.12345", "2401.12345"),
            ("2401.12345v2", "2401.12345v2"),
            ("https://arxiv.org/abs/2401.12345", "2401.12345"),
            ("https://arxiv.org/pdf/2401.12345", "2401.12345"),
            ("https://arxiv.org/pdf/2401.12345.pdf", "2401.12345"),
            ("https://www.alphaxiv.org/overview/2401.12345", "2401.12345"),
            ("https://alphaxiv.org/abs/2401.12345v2", "2401.12345v2"),
            ("https://arxiv.org/abs/2401.12345?foo=bar", "2401.12345"),
        ];
        for (input, want) in cases {
            assert_eq!(parse_paper_id(input), want, "input: {input}");
        }
    }
}
