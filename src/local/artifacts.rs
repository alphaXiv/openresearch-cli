//! Per-project artifacts directory — a plain folder on the user's machine
//! (`<data dir>/artifacts/<project slug>/`). The filesystem is the source of
//! truth: no registry, no upload step. Subfolders with a top-level `report.md`
//! render as reports in the dashboard's Artifacts tab; everything else is
//! browsable files.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{anyhow, Result};
use crate::store::data_dir;

use super::model::LocalProject;

/// Files surfaced by the OS that aren't artifacts.
const IGNORED: &[&str] = &[".DS_Store", "Thumbs.db"];

/// Listing cap — a runaway directory shouldn't stall the 2Hz event loop.
const MAX_ENTRIES: usize = 2000;

/// `<data dir>/artifacts/<slug>/` — slugs are unique per store and filesystem-safe.
pub fn artifacts_dir(project: &LocalProject) -> PathBuf {
    data_dir().join("artifacts").join(&project.slug)
}

/// Create the artifacts dir if missing and return it.
pub fn ensure_dir(project: &LocalProject) -> Result<PathBuf> {
    let dir = artifacts_dir(project);
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow!("Could not create {}: {}", dir.display(), e))?;
    Ok(dir)
}

/// Relative, no `..`/`.` segments, no backslashes — a requested path can't
/// escape the artifacts dir.
pub fn is_safe_artifact_path(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && !p.contains('\\')
        && !p
            .split('/')
            .any(|seg| seg == ".." || seg == "." || seg.is_empty())
}

/// Best-effort content type from a file extension (serving artifact files).
pub fn content_type_for_path(path: &str) -> &'static str {
    match path
        .rsplit('.')
        .next()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("md") => "text/markdown; charset=utf-8",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("pdf") => "application/pdf",
        Some("json") => "application/json",
        Some("csv") => "text/csv",
        Some("txt") => "text/plain; charset=utf-8",
        Some("html") => "text/html; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// A subfolder with a top-level `report.md`, rendered as a report.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactReport {
    /// Folder name — the report's id and path prefix for its files.
    pub name: String,
    pub title: String,
    pub modified_at: i64,
}

/// Any other file in the artifacts dir, by dir-relative path.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactFile {
    pub path: String,
    pub size: u64,
    pub modified_at: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactsListing {
    /// Absolute path of the artifacts dir, shown in the UI so the user can
    /// drop files in.
    pub dir: String,
    pub reports: Vec<ArtifactReport>,
    pub files: Vec<ArtifactFile>,
    pub truncated: bool,
}

fn mtime_ms(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn is_ignored(name: &str) -> bool {
    name.starts_with('.') || IGNORED.contains(&name)
}

/// Report title: first `# ` heading in report.md (skipping YAML frontmatter),
/// else the folder name.
fn report_title(md_path: &Path, fallback: &str) -> String {
    let Ok(text) = std::fs::read_to_string(md_path) else {
        return fallback.to_string();
    };
    let mut lines = text.lines().peekable();
    if lines.peek().map(|l| l.trim()) == Some("---") {
        lines.next();
        for line in lines.by_ref() {
            if line.trim() == "---" {
                break;
            }
        }
    }
    lines
        .find_map(|l| l.strip_prefix("# ").map(|t| t.trim().to_string()))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

/// Recursively collect files under `dir` as `/`-joined paths relative to
/// `base`, up to `MAX_ENTRIES`. Returns true when it hit the cap.
fn collect_files(base: &Path, dir: &Path, out: &mut Vec<ArtifactFile>) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        if out.len() >= MAX_ENTRIES {
            return true;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_ignored(&name) {
            continue;
        }
        let path = entry.path();
        let Ok(md) = entry.metadata() else { continue };
        if md.is_dir() {
            if collect_files(base, &path, out) {
                return true;
            }
        } else if md.is_file() {
            if let Ok(rel) = path.strip_prefix(base) {
                let rel = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                if !rel.is_empty() {
                    out.push(ArtifactFile {
                        path: rel,
                        size: md.len(),
                        modified_at: mtime_ms(&md),
                    });
                }
            }
        }
    }
    false
}

/// Scan the artifacts dir (creating it if missing): report folders first,
/// then every other file.
pub fn list(project: &LocalProject) -> Result<ArtifactsListing> {
    let dir = ensure_dir(project)?;
    let mut reports = Vec::new();
    let mut files = Vec::new();
    let mut truncated = false;

    let entries =
        std::fs::read_dir(&dir).map_err(|e| anyhow!("Could not read {}: {}", dir.display(), e))?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_ignored(&name) {
            continue;
        }
        let path = entry.path();
        let Ok(md) = entry.metadata() else { continue };
        if md.is_dir() {
            let report_md = path.join("report.md");
            if report_md.is_file() {
                let modified = std::fs::metadata(&report_md)
                    .map(|m| mtime_ms(&m))
                    .unwrap_or(0);
                reports.push(ArtifactReport {
                    title: report_title(&report_md, &name),
                    name,
                    modified_at: modified,
                });
            } else {
                truncated |= collect_files(&dir, &path, &mut files);
            }
        } else if md.is_file() {
            files.push(ArtifactFile {
                path: name,
                size: md.len(),
                modified_at: mtime_ms(&md),
            });
        }
    }

    reports.sort_by_key(|r| std::cmp::Reverse(r.modified_at));
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(ArtifactsListing {
        dir: dir.to_string_lossy().into_owned(),
        reports,
        files,
        truncated,
    })
}

/// A report folder's `report.md` body.
pub fn read_report_markdown(project: &LocalProject, report_name: &str) -> Result<String> {
    if !is_safe_artifact_path(report_name) || report_name.contains('/') {
        return Err(anyhow!("invalid report name: {report_name}"));
    }
    let path = artifacts_dir(project).join(report_name).join("report.md");
    std::fs::read_to_string(&path).map_err(|e| anyhow!("Could not read {}: {}", path.display(), e))
}

/// One file in the artifacts dir, by dir-relative path.
pub fn read_file(project: &LocalProject, rel_path: &str) -> Result<Vec<u8>> {
    if !is_safe_artifact_path(rel_path) {
        return Err(anyhow!("invalid artifact path: {rel_path}"));
    }
    let path = artifacts_dir(project).join(rel_path);
    std::fs::read(&path).map_err(|e| anyhow!("Could not read {}: {}", path.display(), e))
}

/// Delete a file or folder (report) in the artifacts dir.
pub fn delete_entry(project: &LocalProject, rel_path: &str) -> Result<()> {
    if !is_safe_artifact_path(rel_path) {
        return Err(anyhow!("invalid artifact path: {rel_path}"));
    }
    let path = artifacts_dir(project).join(rel_path);
    let md = std::fs::symlink_metadata(&path)
        .map_err(|e| anyhow!("Could not stat {}: {}", path.display(), e))?;
    if md.is_dir() {
        std::fs::remove_dir_all(&path)
    } else {
        std::fs::remove_file(&path)
    }
    .map_err(|e| anyhow!("Could not delete {}: {}", path.display(), e))
}

/// Cheap change fingerprint (paths + sizes + mtimes) for the SSE diff loop.
/// A missing dir hashes to a stable value, so first creation is a change.
pub fn fingerprint(project: &LocalProject) -> u64 {
    let dir = artifacts_dir(project);
    let mut hasher = DefaultHasher::new();
    hash_dir(&dir, &dir, &mut hasher, &mut 0);
    hasher.finish()
}

fn hash_dir(base: &Path, dir: &Path, hasher: &mut DefaultHasher, seen: &mut usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if *seen >= MAX_ENTRIES {
            return;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_ignored(&name) {
            continue;
        }
        let Ok(md) = entry.metadata() else { continue };
        *seen += 1;
        if let Ok(rel) = entry.path().strip_prefix(base) {
            rel.to_string_lossy().hash(hasher);
        }
        if md.is_dir() {
            hash_dir(base, &entry.path(), hasher, seen);
        } else {
            md.len().hash(hasher);
            mtime_ms(&md).hash(hasher);
        }
    }
}
