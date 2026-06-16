//! Project reports: upload a report folder (report.md + images) to a project,
//! or list a project's existing reports.
//!
//! A report on disk is just a folder — typically a `report.md` plus an
//! `images/` subfolder, exactly the shape the autoresearch agent writes. Upload
//! creates the report row, then PUTs each file directly to storage using the
//! presigned URLs the API returns (bytes never transit the API).

use std::path::{Path, PathBuf};

use crate::client::{create_report, list_reports, upload_to_presigned, CreateReportBody};
use crate::error::{anyhow, require_credentials, Result};

pub async fn run(args: crate::ReportArgs) -> Result<()> {
    match args.command {
        crate::ReportCommand::Upload {
            project_id,
            folder,
            title,
        } => upload(&project_id, &folder, title).await,
        crate::ReportCommand::List { project_id } => list(&project_id).await,
    }
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
