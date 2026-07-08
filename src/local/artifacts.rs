//! Per-project artifacts directory — a plain folder on the user's machine
//! (`<data dir>/artifacts/<project slug>/`). The filesystem is the source of
//! truth: no registry, no upload step. The dashboard's Artifacts tab is an
//! explorer over this folder; a folder with a top-level `report.md` is still
//! just a folder, it only additionally renders as a report.

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

/// One node of the artifacts tree: a file or a directory with its children.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactEntry {
    pub name: String,
    /// Dir-relative, `/`-joined — the id for file/report/delete endpoints.
    pub path: String,
    pub is_dir: bool,
    /// 0 for directories.
    pub size: u64,
    pub modified_at: i64,
    /// Set when this dir holds a top-level `report.md` — the UI offers a
    /// rendered-report view on top of the normal folder row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report_title: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ArtifactEntry>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactsListing {
    /// Absolute path of the artifacts dir, shown in the UI so the user can
    /// drop files in.
    pub dir: String,
    pub entries: Vec<ArtifactEntry>,
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

/// Recursively build the tree under `dir`, counting nodes against
/// `MAX_ENTRIES`. Returns (children, hit_cap).
fn collect_tree(dir: &Path, rel_prefix: &str, seen: &mut usize) -> (Vec<ArtifactEntry>, bool) {
    let mut out = Vec::new();
    let mut truncated = false;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (out, false);
    };
    for entry in entries.flatten() {
        if *seen >= MAX_ENTRIES {
            return (out, true);
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_ignored(&name) {
            continue;
        }
        let Ok(md) = entry.metadata() else { continue };
        *seen += 1;
        let rel = if rel_prefix.is_empty() {
            name.clone()
        } else {
            format!("{rel_prefix}/{name}")
        };
        if md.is_dir() {
            let report_md = entry.path().join("report.md");
            let report_title = report_md
                .is_file()
                .then(|| report_title(&report_md, &name));
            let (children, hit) = collect_tree(&entry.path(), &rel, seen);
            truncated |= hit;
            out.push(ArtifactEntry {
                name,
                path: rel,
                is_dir: true,
                size: 0,
                modified_at: mtime_ms(&md),
                report_title,
                children,
            });
        } else if md.is_file() {
            out.push(ArtifactEntry {
                name,
                path: rel,
                is_dir: false,
                size: md.len(),
                modified_at: mtime_ms(&md),
                report_title: None,
                children: Vec::new(),
            });
        }
    }
    // Dirs first, then files, each alphabetical — stable explorer order.
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
    (out, truncated)
}

/// Scan the artifacts dir (creating it if missing) into a file tree.
pub fn list(project: &LocalProject) -> Result<ArtifactsListing> {
    let dir = ensure_dir(project)?;
    let mut seen = 0;
    let (entries, truncated) = collect_tree(&dir, "", &mut seen);
    Ok(ArtifactsListing {
        dir: dir.to_string_lossy().into_owned(),
        entries,
        truncated,
    })
}

/// A report folder's `report.md` body, by dir-relative folder path.
pub fn read_report_markdown(project: &LocalProject, folder: &str) -> Result<String> {
    if !is_safe_artifact_path(folder) {
        return Err(anyhow!("invalid report path: {folder}"));
    }
    let path = artifacts_dir(project).join(folder).join("report.md");
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
