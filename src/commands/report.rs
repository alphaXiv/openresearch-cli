//! Project reports: upload a report folder (report.md + images) to a project,
//! or list a project's existing reports.
//!
//! A report on disk is just a folder — typically a `report.md` plus an
//! `images/` subfolder, exactly the shape the autoresearch agent writes. Upload
//! creates the report row, then PUTs each file directly to storage using the
//! presigned URLs the API returns (bytes never transit the API).

use std::path::{Path, PathBuf};

use crate::client::{
    create_report, download_report_file, get_report, list_reports, upload_to_presigned,
    CreateReportBody,
};
use crate::config::Credentials;
use crate::error::{anyhow, require_credentials, Result};

pub async fn run(args: crate::ReportArgs) -> Result<()> {
    match args.command {
        crate::ReportCommand::Upload {
            project_id,
            folder,
            title,
        } => upload(&project_id, &folder, title).await,
        crate::ReportCommand::List { project_id } => list(&project_id).await,
        crate::ReportCommand::Show { project_id, report } => show(&project_id, &report).await,
        crate::ReportCommand::Download {
            project_id,
            report,
            dir,
        } => download(&project_id, &report, &dir).await,
    }
}

/// Resolve a report id-or-slug to its id, erroring clearly if it isn't found.
/// We always list first so a stale ref gives a helpful message, not a raw 404.
async fn resolve_report_id(creds: &Credentials, project_id: &str, report: &str) -> Result<String> {
    let reports = list_reports(creds, project_id).await?.reports;
    reports
        .iter()
        .find(|r| r.id == report || r.slug == report)
        .map(|r| r.id.clone())
        .ok_or_else(|| {
            anyhow!(
                "No report {:?} in this project. List them with: orx report list {}",
                report,
                project_id
            )
        })
}

/// `orx report show <projectId> <reportId|slug>` — print a report's markdown
/// body to stdout. Accepts a report id or its slug (resolved via the list).
async fn show(project_id: &str, report: &str) -> Result<()> {
    let creds = require_credentials().await;
    let report_id = resolve_report_id(&creds, project_id, report).await?;

    let detail = get_report(&creds, project_id, &report_id).await?;
    if detail.markdown.is_empty() {
        return Err(anyhow!(
            "Report {:?} has no markdown body (report.md was never uploaded).",
            detail.report.title
        ));
    }
    print!("{}", detail.markdown);
    if !detail.markdown.ends_with('\n') {
        println!();
    }
    Ok(())
}

/// `orx report download <projectId> <reportId|slug> <dir>` — write a report's
/// `report.md` (raw, frontmatter intact) plus every image it references into
/// `dir`, reconstructing the same folder shape `upload` consumes. This is what
/// lets a local publish step feed the report back into the alphaXiv ingest.
async fn download(project_id: &str, report: &str, dir: &str) -> Result<()> {
    let creds = require_credentials().await;
    let report_id = resolve_report_id(&creds, project_id, report).await?;

    let detail = get_report(&creds, project_id, &report_id).await?;
    if detail.markdown.is_empty() {
        return Err(anyhow!(
            "Report {:?} has no markdown body (report.md was never uploaded).",
            detail.report.title
        ));
    }

    let root = PathBuf::from(dir);
    std::fs::create_dir_all(&root)
        .map_err(|e| anyhow!("Could not create {}: {}", root.display(), e))?;

    // report.md, byte-for-byte (the markdown the API returns is the stored file,
    // YAML frontmatter included — the ingest reads `repo`/`gpu`/`count` from it).
    let md_path = root.join("report.md");
    std::fs::write(&md_path, detail.markdown.as_bytes())
        .map_err(|e| anyhow!("Could not write {}: {}", md_path.display(), e))?;
    println!("  wrote report.md");

    // Pull every report-relative file the markdown links to (images, mostly).
    // There's no list-files endpoint, so the references in report.md are the
    // manifest — which is exactly the set that has to exist for it to render.
    let mut downloaded = 0usize;
    for rel in report_relative_links(&detail.markdown) {
        if !is_safe_report_path(&rel) {
            continue;
        }
        let bytes = match download_report_file(&creds, project_id, &report_id, &rel).await {
            Ok(b) => b,
            // A broken link in the markdown shouldn't abort the whole download;
            // surface it and keep going.
            Err(e) => {
                eprintln!("  ! skipped {} ({})", rel, e);
                continue;
            }
        };
        let out = root.join(&rel);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow!("Could not create {}: {}", parent.display(), e))?;
        }
        std::fs::write(&out, &bytes)
            .map_err(|e| anyhow!("Could not write {}: {}", out.display(), e))?;
        println!("  wrote {}", rel);
        downloaded += 1;
    }

    println!(
        "\u{2713} Downloaded report to {} (report.md + {} file{})",
        root.display(),
        downloaded,
        if downloaded == 1 { "" } else { "s" }
    );
    Ok(())
}

/// Extract the report-relative link/image targets from markdown — the `target`
/// in every `](target)` (covers `![alt](images/x.png)` and `[text](file)`).
/// Filters out absolute URLs, anchors, and absolute paths, leaving the local
/// files the report bundles. Deduplicated, order preserved.
fn report_relative_links(md: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let bytes = md.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b']' && bytes[i + 1] == b'(' {
            let start = i + 2;
            if let Some(rel) = bytes[start..].iter().position(|&b| b == b')') {
                let inner = &md[start..start + rel];
                // Drop an optional `"title"` after the URL: `(path "t")`.
                let target = inner.split_whitespace().next().unwrap_or("").trim();
                if is_local_target(target) && !out.iter().any(|p| p == target) {
                    out.push(target.to_string());
                }
                i = start + rel + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// A link target that points at a file bundled in the report (not the web).
fn is_local_target(t: &str) -> bool {
    !t.is_empty()
        && !t.starts_with('#')
        && !t.starts_with('/')
        && !t.contains("://")
        && !t.starts_with("//")
        && !t.starts_with("mailto:")
        && !t.starts_with("data:")
}

/// Mirror of the server's `isSafeReportPath`: relative, no `..`/`.` segments,
/// no backslashes — so a malicious markdown link can't escape `dir`.
fn is_safe_report_path(p: &str) -> bool {
    !p.starts_with('/')
        && !p.contains('\\')
        && !p.split('/').any(|seg| seg == ".." || seg == ".")
}

async fn list(project_id: &str) -> Result<()> {
    let creds = require_credentials().await;
    let reports = list_reports(&creds, project_id).await?.reports;
    if reports.is_empty() {
        println!("No reports yet.");
        return Ok(());
    }
    for r in reports {
        println!("{}  {}  ({})", r.id, r.title, r.created_at);
    }
    Ok(())
}

// Files surfaced by the OS that aren't part of a report.
const IGNORED: &[&str] = &[".DS_Store", "Thumbs.db"];

async fn upload(project_id: &str, folder: &str, title: Option<String>) -> Result<()> {
    let creds = require_credentials().await;

    let root = PathBuf::from(folder);
    if !root.is_dir() {
        return Err(anyhow!("Not a directory: {}", folder));
    }

    // Collect every file under the folder as a report-relative POSIX path.
    let mut rel_paths: Vec<String> = Vec::new();
    collect_files(&root, &root, &mut rel_paths)?;
    rel_paths.retain(|p| {
        let name = p.rsplit('/').next().unwrap_or(p);
        !IGNORED.contains(&name)
    });

    if rel_paths.is_empty() {
        return Err(anyhow!("No files found in {}", folder));
    }
    if !rel_paths.iter().any(|p| p == "report.md") {
        return Err(anyhow!(
            "{} must contain a report.md at its top level",
            folder
        ));
    }

    // Title defaults to the folder name.
    let title = title.unwrap_or_else(|| {
        root.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("report")
            .to_string()
    });

    let result = create_report(
        &creds,
        project_id,
        &CreateReportBody {
            title: title.clone(),
            slug: None,
            files: rel_paths.clone(),
        },
    )
    .await?;

    // Upload each file to its presigned URL.
    for slot in &result.uploads {
        let abs = root.join(&slot.path);
        let bytes =
            std::fs::read(&abs).map_err(|e| anyhow!("Could not read {}: {}", abs.display(), e))?;
        upload_to_presigned(&slot.url, &slot.content_type, bytes).await?;
        println!("  uploaded {}", slot.path);
    }

    println!("\u{2713} Uploaded report");
    println!("  id:    {}", result.report.id);
    println!("  title: {}", result.report.title);
    println!("  files: {}", result.uploads.len());
    Ok(())
}

/// Recursively collect files under `dir`, pushing each as a `/`-joined path
/// relative to `base`.
fn collect_files(base: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    for entry in
        std::fs::read_dir(dir).map_err(|e| anyhow!("Could not read {}: {}", dir.display(), e))?
    {
        let entry = entry.map_err(|e| anyhow!("Could not read entry: {}", e))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|e| anyhow!("Could not stat {}: {}", path.display(), e))?;
        if file_type.is_dir() {
            collect_files(base, &path, out)?;
        } else if file_type.is_file() {
            if let Ok(rel) = path.strip_prefix(base) {
                let rel = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                if !rel.is_empty() {
                    out.push(rel);
                }
            }
        }
    }
    Ok(())
}
