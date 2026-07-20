//! Per-project files directory — a plain folder on the user's machine
//! (`<data dir>/files/<project slug>/`). The filesystem is the source of
//! truth: no registry, no upload step. The dashboard's Files tab is an
//! explorer over this folder; a folder with a top-level `report.md` is still
//! just a folder, it only additionally renders as a report.
//!
//! Layout convention (enforced by prompt + UI grouping, not by validation):
//! every top-level folder corresponds to an experiment, named by its slug —
//! `<slug>/report.md` plus figures. The reserved `project/` namespace holds
//! cross-experiment syntheses and anything not tied to one node (its name is
//! kept out of the experiment-slug space by `experiments::unique_slug`),
//! including `project/memory.md` — the agent's persisted project memory,
//! inlined into the playbook (see `memory.rs`).

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{anyhow, Result};
use crate::store::data_dir;

use super::model::{LocalExperiment, LocalProject};

/// Top-level folder reserved for project-wide reports (cross-experiment
/// syntheses, lit reviews). Never a valid experiment slug.
pub const PROJECT_NAMESPACE: &str = "project";

/// Files surfaced by the OS that aren't the user's or the agent's.
const IGNORED: &[&str] = &[".DS_Store", "Thumbs.db"];

/// Listing cap — a runaway directory shouldn't stall the 2Hz event loop.
const MAX_ENTRIES: usize = 2000;

/// `<data dir>/files/`, migrating the pre-rename `artifacts/` root in place
/// the first time it's touched (the tab and dir used to be called Artifacts).
fn files_root() -> PathBuf {
    let root = data_dir().join("files");
    let legacy = data_dir().join("artifacts");
    if !root.exists() && legacy.is_dir() {
        // Same filesystem (sibling dirs), so a plain rename; on failure fall
        // through — ensure_dir will create the new root and the legacy dir
        // simply stops being served.
        let _ = std::fs::rename(&legacy, &root);
    }
    root
}

/// `<data dir>/files/<slug>/` — slugs are unique per store and filesystem-safe.
pub fn files_dir(project: &LocalProject) -> PathBuf {
    files_root().join(&project.slug)
}

/// Create the project's files dir if missing and return it.
pub fn ensure_dir(project: &LocalProject) -> Result<PathBuf> {
    let dir = files_dir(project);
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow!("Could not create {}: {}", dir.display(), e))?;
    Ok(dir)
}

/// Relative, no `..`/`.` segments, no backslashes — a requested path can't
/// escape the files dir. Lexical only; symlink containment is enforced by
/// `resolve_contained`.
pub fn is_safe_rel_path(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && !p.contains('\\')
        && !p
            .split('/')
            .any(|seg| seg == ".." || seg == "." || seg.is_empty())
}

/// True when `path` resolves (following symlinks) to a location inside
/// `canonical_base`. Anything that fails to resolve is treated as outside.
fn resolves_inside(canonical_base: &Path, path: &Path) -> bool {
    path.canonicalize()
        .map(|c| c.starts_with(canonical_base))
        .unwrap_or(false)
}

/// Join `rel_path` onto `base` and resolve it, requiring the result to stay
/// inside `base` once every symlink is followed. `is_safe_rel_path` already
/// blocks lexical escapes (`..`); this closes the remaining hole — a symlink
/// inside the dir pointing outside it. Internal symlinks still work.
fn resolve_contained(base: &Path, rel_path: &str) -> Result<PathBuf> {
    if !is_safe_rel_path(rel_path) {
        return Err(anyhow!("invalid file path: {rel_path}"));
    }
    let canonical_base = base
        .canonicalize()
        .map_err(|e| anyhow!("Could not resolve {}: {}", base.display(), e))?;
    let path = canonical_base.join(rel_path);
    let canonical = path
        .canonicalize()
        .map_err(|e| anyhow!("Could not read {}: {}", path.display(), e))?;
    if !canonical.starts_with(&canonical_base) {
        return Err(anyhow!("path escapes the files dir: {rel_path}"));
    }
    Ok(canonical)
}

/// Best-effort content type from a file extension (serving files).
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

/// The experiment a top-level folder corresponds to (folder name == slug),
/// so the tab can render folders grouped by experiment.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileExperiment {
    pub id: String,
    pub slug: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub branch_name: String,
    /// The experiment's most recent run status, if it has ever run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_run_status: Option<String>,
}

/// One node of the files tree: a file or a directory with its children.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
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
    /// Top-level dirs only: the experiment this folder is named for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub experiment: Option<FileExperiment>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<FileEntry>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FilesListing {
    /// Absolute path of the files dir, shown in the UI so the user can drop
    /// files in.
    pub dir: String,
    pub entries: Vec<FileEntry>,
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
/// `MAX_ENTRIES`. Returns (children, hit_cap). Symlinks resolving outside
/// `canonical_base` are skipped — the serve endpoints would refuse them.
fn collect_tree(
    canonical_base: &Path,
    dir: &Path,
    rel_prefix: &str,
    seen: &mut usize,
) -> (Vec<FileEntry>, bool) {
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
        let Ok(ft) = entry.file_type() else { continue };
        // DirEntry::metadata never follows symlinks; a contained symlink is
        // listed as its target (followed metadata), an escaping one skipped.
        let md = if ft.is_symlink() {
            if !resolves_inside(canonical_base, &entry.path()) {
                continue;
            }
            let Ok(md) = std::fs::metadata(entry.path()) else {
                continue;
            };
            md
        } else {
            let Ok(md) = entry.metadata() else { continue };
            md
        };
        *seen += 1;
        let rel = if rel_prefix.is_empty() {
            name.clone()
        } else {
            format!("{rel_prefix}/{name}")
        };
        if md.is_dir() {
            let report_md = entry.path().join("report.md");
            let report_title = (report_md.is_file() && resolves_inside(canonical_base, &report_md))
                .then(|| report_title(&report_md, &name));
            let (children, hit) = collect_tree(canonical_base, &entry.path(), &rel, seen);
            truncated |= hit;
            out.push(FileEntry {
                name,
                path: rel,
                is_dir: true,
                size: 0,
                modified_at: mtime_ms(&md),
                report_title,
                experiment: None,
                children,
            });
        } else if md.is_file() {
            out.push(FileEntry {
                name,
                path: rel,
                is_dir: false,
                size: md.len(),
                modified_at: mtime_ms(&md),
                report_title: None,
                experiment: None,
                children: Vec::new(),
            });
        }
    }
    // Dirs first, then files, each alphabetical — stable explorer order.
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
    (out, truncated)
}

/// Scan the files dir (creating it if missing) into a file tree. Top-level
/// folders named for an experiment slug are decorated with that experiment
/// (plus its latest run status from `latest_status`, keyed by experiment id)
/// so the tab can group by experiment.
pub fn list(
    project: &LocalProject,
    experiments: &[LocalExperiment],
    latest_status: &HashMap<String, String>,
) -> Result<FilesListing> {
    let dir = ensure_dir(project)?;
    let canonical = dir
        .canonicalize()
        .map_err(|e| anyhow!("Could not resolve {}: {}", dir.display(), e))?;
    let mut seen = 0;
    let (mut entries, truncated) = collect_tree(&canonical, &canonical, "", &mut seen);
    let by_slug: HashMap<&str, &LocalExperiment> =
        experiments.iter().map(|e| (e.slug.as_str(), e)).collect();
    for entry in entries.iter_mut().filter(|e| e.is_dir) {
        if let Some(exp) = by_slug.get(entry.name.as_str()) {
            entry.experiment = Some(FileExperiment {
                id: exp.id.clone(),
                slug: exp.slug.clone(),
                title: exp.title.clone(),
                branch_name: exp.branch_name.clone(),
                latest_run_status: latest_status.get(&exp.id).cloned(),
            });
        }
    }
    Ok(FilesListing {
        dir: dir.to_string_lossy().into_owned(),
        entries,
        truncated,
    })
}

/// A report folder's `report.md` body, by dir-relative folder path.
pub fn read_report_markdown(project: &LocalProject, folder: &str) -> Result<String> {
    if !is_safe_rel_path(folder) {
        return Err(anyhow!("invalid report path: {folder}"));
    }
    let path = resolve_contained(&files_dir(project), &format!("{folder}/report.md"))?;
    std::fs::read_to_string(&path).map_err(|e| anyhow!("Could not read {}: {}", path.display(), e))
}

/// One file in the files dir, by dir-relative path.
pub fn read_file(project: &LocalProject, rel_path: &str) -> Result<Vec<u8>> {
    let path = resolve_contained(&files_dir(project), rel_path)?;
    std::fs::read(&path).map_err(|e| anyhow!("Could not read {}: {}", path.display(), e))
}

/// Delete a file or folder (report) in the files dir.
///
/// The final component is deleted literally — a symlink is removed, never
/// followed — but every parent segment must resolve inside the files dir, or
/// `a/b` with `a -> /elsewhere` would delete outside it.
pub fn delete_entry(project: &LocalProject, rel_path: &str) -> Result<()> {
    if !is_safe_rel_path(rel_path) {
        return Err(anyhow!("invalid file path: {rel_path}"));
    }
    let base = files_dir(project);
    let parent = match rel_path.rsplit_once('/') {
        Some((parent_rel, _)) => resolve_contained(&base, parent_rel)?,
        None => base
            .canonicalize()
            .map_err(|e| anyhow!("Could not resolve {}: {}", base.display(), e))?,
    };
    let name = rel_path.rsplit('/').next().unwrap_or(rel_path);
    let path = parent.join(name);
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
    let mut hasher = DefaultHasher::new();
    if let Ok(canonical) = files_dir(project).canonicalize() {
        hash_dir(&canonical, &canonical, &mut hasher, &mut 0);
    }
    hasher.finish()
}

/// Hash the tree under `dir`, skipping (like `collect_tree`) symlinks that
/// resolve outside `base`, so the fingerprint tracks exactly what's listed.
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
        let Ok(ft) = entry.file_type() else { continue };
        // Mirror collect_tree: contained symlinks hash as their target,
        // escaping ones are invisible.
        let md = if ft.is_symlink() {
            if !resolves_inside(base, &entry.path()) {
                continue;
            }
            let Ok(md) = std::fs::metadata(entry.path()) else {
                continue;
            };
            md
        } else {
            let Ok(md) = entry.metadata() else { continue };
            md
        };
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// Fresh scratch dir with a `base/` (the files dir under test) and an
    /// `outside/` holding a file symlinks will try to escape to.
    fn scratch() -> (PathBuf, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("orx-files-{}", uuid::Uuid::new_v4()));
        let base = root.join("base");
        let outside = root.join("outside");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "secret").unwrap();
        (root, base, outside)
    }

    #[test]
    fn rejects_lexical_escapes() {
        let (root, base, _) = scratch();
        for bad in ["../x", "/etc/passwd", "a/../b", "a/./b", "", "a\\b"] {
            assert!(resolve_contained(&base, bad).is_err(), "accepted {bad:?}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn blocks_symlink_file_escape() {
        let (root, base, outside) = scratch();
        symlink(outside.join("secret.txt"), base.join("link.txt")).unwrap();
        assert!(resolve_contained(&base, "link.txt").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn blocks_symlink_dir_escape() {
        let (root, base, outside) = scratch();
        symlink(&outside, base.join("sub")).unwrap();
        assert!(resolve_contained(&base, "sub/secret.txt").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn allows_internal_symlink() {
        let (root, base, _) = scratch();
        std::fs::write(base.join("real.txt"), "data").unwrap();
        symlink("real.txt", base.join("alias.txt")).unwrap();
        let resolved = resolve_contained(&base, "alias.txt").unwrap();
        assert_eq!(std::fs::read_to_string(resolved).unwrap(), "data");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn allows_regular_files() {
        let (root, base, _) = scratch();
        std::fs::create_dir(base.join("exp")).unwrap();
        std::fs::write(base.join("exp/report.md"), "# T").unwrap();
        assert!(resolve_contained(&base, "exp/report.md").is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn listing_skips_escaping_symlinks_keeps_internal() {
        let (root, base, outside) = scratch();
        std::fs::write(base.join("real.txt"), "data").unwrap();
        symlink("real.txt", base.join("alias.txt")).unwrap();
        symlink(outside.join("secret.txt"), base.join("leak.txt")).unwrap();
        symlink(&outside, base.join("leakdir")).unwrap();
        let canonical = base.canonicalize().unwrap();
        let (entries, truncated) = collect_tree(&canonical, &canonical, "", &mut 0);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["alias.txt", "real.txt"]);
        assert!(!truncated);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn listing_hides_report_title_behind_escaping_symlink() {
        let (root, base, outside) = scratch();
        std::fs::write(outside.join("report.md"), "# Leaked heading").unwrap();
        std::fs::create_dir(base.join("exp")).unwrap();
        symlink(outside.join("report.md"), base.join("exp/report.md")).unwrap();
        let canonical = base.canonicalize().unwrap();
        let (entries, _) = collect_tree(&canonical, &canonical, "", &mut 0);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].report_title, None);
        // The dangling entry itself is skipped too, not just the title.
        assert!(entries[0].children.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
}
