//! The `paper` command — fetch a paper from whichever source its id belongs to.
//!
//! Public endpoints, no token required. The source is auto-detected from the id
//! (override with `--source`):
//!   - **alphaXiv** (arXiv id / URL): a machine-readable report (default, ≈10 KB)
//!     or the full extracted text (`--full`). Markdown to stdout. If alphaXiv has
//!     a GitHub repo linked, a `GitHub: <url>` line is printed first.
//!   - **bioRxiv** (`10.1101/…` DOI): title/authors/date + abstract, with links
//!     to the DOI and full-text PDF.
//!   - **OpenAlex** (`W…` id or any other DOI): title/authors/date/citations +
//!     abstract, with DOI and open-access PDF links.
//!
//! OpenAlex/bioRxiv have no *extracted* full text, so `--full` on those just
//! points you at the PDF.

use crate::client::{
    fetch_biorxiv, fetch_openalex_work, fetch_paper_github, fetch_paper_markdown, BiorxivDetail,
    OpenAlexWork,
};
use crate::error::{anyhow, Result};
use crate::LitSource;

pub async fn run(args: crate::PaperArgs) -> Result<()> {
    let source = args.source.unwrap_or_else(|| detect_source(&args.id));
    ensure_source_enabled(source, &crate::config::disabled_lit_sources())?;
    match source {
        LitSource::Alphaxiv => run_alphaxiv(&args).await,
        LitSource::Openalex => run_openalex(&args.id, args.full).await,
        LitSource::Biorxiv => run_biorxiv(&args.id, args.full).await,
        LitSource::Youcom => run_youcom(&args.id),
    }
}

/// A source disabled by the user refuses to fetch too, so a
/// source turned off is off everywhere — the same gate `orx lit` applies.
fn ensure_source_enabled(source: LitSource, disabled: &[String]) -> Result<()> {
    if disabled.iter().any(|d| d == source.as_str()) {
        return Err(anyhow!(
            "{} is disabled by your OpenResearch literature-source configuration. Re-enable it to fetch this paper.",
            source.display_name()
        ));
    }
    Ok(())
}

async fn run_alphaxiv(args: &crate::PaperArgs) -> Result<()> {
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

async fn run_openalex(raw: &str, full: bool) -> Result<()> {
    match fetch_openalex_work(raw).await? {
        Some(w) => {
            print_openalex(&w, full);
            Ok(())
        }
        None => Err(anyhow!(
            "No OpenAlex work found for {raw:?}. Check the id/DOI, or search with `orx lit --source openalex`."
        )),
    }
}

async fn run_biorxiv(raw: &str, full: bool) -> Result<()> {
    let doi = biorxiv_doi(&extract_doi(raw).unwrap_or_else(|| raw.trim().to_string()));
    match fetch_biorxiv(&doi).await? {
        Some(d) => {
            print_biorxiv(&d, full);
            Ok(())
        }
        None => Err(anyhow!(
            "No bioRxiv preprint found for {doi}. If it's a medRxiv or non-bioRxiv DOI, try `orx paper {doi} --source openalex`; or search with `orx lit --source biorxiv`."
        )),
    }
}

fn run_youcom(url: &str) -> Result<()> {
    // For You.com web search results, the "id" is actually a URL
    // Since these are web pages, not academic papers, we just provide the URL
    println!("# Web Search Result");
    println!("**URL:** {url}");
    println!();
    println!("This is a web search result from You.com. To view the content, please visit the URL above.");
    println!();
    println!("To search for more related content, use: `orx lit --source youcom \"<your query>\"`");
    Ok(())
}

fn print_openalex(w: &OpenAlexWork, full: bool) {
    if let Some(t) = &w.title {
        println!("# {t}");
    }
    let authors = w.author_names();
    if !authors.is_empty() {
        println!("{}", format_authors(&authors));
    }
    let mut meta = Vec::new();
    if let Some(d) = &w.publication_date {
        meta.push(d.clone());
    }
    if let Some(c) = w.cited_by_count {
        meta.push(format!("{c} citations"));
    }
    if !meta.is_empty() {
        println!("{}", meta.join(" · "));
    }
    if let Some(doi) = w.doi_bare() {
        println!("DOI: https://doi.org/{doi}");
    }
    if let Some(pdf) = w.oa_url() {
        println!("PDF: {pdf}");
    }
    println!();
    let abs = w.abstract_text();
    if abs.is_empty() {
        println!("(No abstract available from OpenAlex.)");
    } else {
        println!("{abs}");
    }
    if full {
        eprintln!("OpenAlex has metadata + abstract only — open the PDF/DOI above for full text.");
    }
}

fn print_biorxiv(d: &BiorxivDetail, full: bool) {
    println!("# {}", d.title);
    if !d.authors.is_empty() {
        println!("{}", d.authors);
    }
    let mut meta = Vec::new();
    if !d.date.is_empty() {
        meta.push(d.date.clone());
    }
    if !d.category.is_empty() {
        meta.push(d.category.clone());
    }
    if !d.version.is_empty() {
        meta.push(format!("v{}", d.version));
    }
    if !meta.is_empty() {
        println!("{}", meta.join(" · "));
    }
    if !d.doi.is_empty() {
        println!("DOI: https://doi.org/{}", d.doi);
        let ver = if d.version.is_empty() {
            String::new()
        } else {
            format!("v{}", d.version)
        };
        println!(
            "Full text: https://www.biorxiv.org/content/{}{}.full",
            d.doi, ver
        );
    }
    if !d.published.is_empty() && d.published != "NA" {
        println!("Published: https://doi.org/{}", d.published);
    }
    println!();
    if d.abstract_.is_empty() {
        println!("(No abstract available from bioRxiv.)");
    } else {
        println!("{}", d.abstract_);
    }
    if full {
        eprintln!(
            "bioRxiv has metadata + abstract only — open the Full text link above for the PDF."
        );
    }
}

/// Join author names, capping a long list so the header stays readable.
fn format_authors(names: &[String]) -> String {
    const MAX: usize = 12;
    if names.len() <= MAX {
        names.join(", ")
    } else {
        format!(
            "{}, … (+{} more)",
            names[..MAX].join(", "),
            names.len() - MAX
        )
    }
}

/// Decide which source an id belongs to, from its shape. Host hints
/// (`biorxiv.org`, `openalex.org`) win first; then a `10.1101/…` DOI → bioRxiv,
/// any other DOI → OpenAlex, a bare `W…` id → OpenAlex; everything else defaults
/// to alphaXiv (arXiv ids and URLs), preserving prior behavior.
fn detect_source(input: &str) -> LitSource {
    let lower = input.trim().to_ascii_lowercase();
    if lower.contains("biorxiv.org") {
        return LitSource::Biorxiv;
    }
    if lower.contains("openalex.org") {
        return LitSource::Openalex;
    }
    if let Some(doi) = extract_doi(input) {
        return if doi.starts_with("10.1101/") {
            LitSource::Biorxiv
        } else {
            LitSource::Openalex
        };
    }
    let last = input.trim().rsplit('/').next().unwrap_or("");
    if is_openalex_id(last) {
        return LitSource::Openalex;
    }
    LitSource::Alphaxiv
}

/// A bare OpenAlex work id: `W`/`w` followed by digits.
fn is_openalex_id(s: &str) -> bool {
    matches!(s.chars().next(), Some('W') | Some('w'))
        && s.len() > 1
        && s[1..].chars().all(|c| c.is_ascii_digit())
}

/// Pull a DOI out of a raw id or URL, or `None` if there isn't one. A real DOI
/// is `10.<registrant>/<suffix>` — the `/` is mandatory, which is what
/// distinguishes it from an arXiv id whose October (`MM=10`) form also contains
/// the substring `10.` (e.g. `2410.12345`) but never a slash. Keeps any trailing
/// bioRxiv content-URL suffix (`v2.full`) — [`biorxiv_doi`] strips that when the
/// DOI is handed to the bioRxiv API.
fn extract_doi(input: &str) -> Option<String> {
    let s = input.trim();
    let s = s.split_once("doi.org/").map(|(_, r)| r).unwrap_or(s);
    let s = s.strip_prefix("doi:").unwrap_or(s);
    let idx = s.find("10.")?;
    let doi = s[idx..].split(['?', '#']).next().unwrap_or(&s[idx..]);
    let doi = doi.trim_end_matches('/');
    doi.contains('/').then(|| doi.to_string())
}

/// bioRxiv's details API wants a versionless DOI. Strip a trailing content-URL
/// suffix (`v2`, `v2.full`, `v2.full.pdf`). bioRxiv DOIs are date-numeric, so the
/// last `v` before digits is unambiguously the version marker.
fn biorxiv_doi(doi: &str) -> String {
    match doi.rsplit_once('v') {
        Some((head, tail)) if tail.starts_with(|c: char| c.is_ascii_digit()) => head.to_string(),
        _ => doi.to_string(),
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
    use super::{biorxiv_doi, detect_source, ensure_source_enabled, extract_doi, parse_paper_id};
    use crate::LitSource;

    #[test]
    fn enforces_disabled_sources() {
        assert!(ensure_source_enabled(LitSource::Biorxiv, &[]).is_ok());
        let disabled = vec!["biorxiv".to_string()];
        assert!(ensure_source_enabled(LitSource::Biorxiv, &disabled).is_err());
        assert!(ensure_source_enabled(LitSource::Alphaxiv, &disabled).is_ok());
    }

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

    #[test]
    fn detects_source_from_id_shape() {
        let cases = [
            ("2401.12345", LitSource::Alphaxiv),
            ("2401.12345v2", LitSource::Alphaxiv),
            ("https://arxiv.org/abs/2401.12345", LitSource::Alphaxiv),
            // October arXiv ids contain the substring "10." but no slash — they
            // must not be mistaken for DOIs (e.g. 1810.04805 = BERT).
            ("2410.12345", LitSource::Alphaxiv),
            ("1810.04805", LitSource::Alphaxiv),
            ("https://arxiv.org/abs/2210.03629", LitSource::Alphaxiv),
            ("https://arxiv.org/pdf/2410.12345.pdf", LitSource::Alphaxiv),
            ("10.1101/2020.09.09.20191205", LitSource::Biorxiv),
            (
                "https://www.biorxiv.org/content/10.1101/2020.09.09.20191205v1",
                LitSource::Biorxiv,
            ),
            ("10.1038/nature14539", LitSource::Openalex),
            ("https://doi.org/10.1038/nature14539", LitSource::Openalex),
            ("W2919115771", LitSource::Openalex),
            ("https://openalex.org/W2919115771", LitSource::Openalex),
        ];
        for (input, want) in cases {
            assert_eq!(detect_source(input), want, "input: {input}");
        }
    }

    #[test]
    fn extracts_and_versionless_biorxiv_doi() {
        assert_eq!(
            extract_doi("https://www.biorxiv.org/content/10.1101/2020.09.09.20191205v2.full"),
            Some("10.1101/2020.09.09.20191205v2.full".to_string())
        );
        assert_eq!(
            biorxiv_doi("10.1101/2020.09.09.20191205v2.full"),
            "10.1101/2020.09.09.20191205"
        );
        // Versionless DOI is left untouched.
        assert_eq!(
            biorxiv_doi("10.1101/2020.09.09.20191205"),
            "10.1101/2020.09.09.20191205"
        );
    }
}
