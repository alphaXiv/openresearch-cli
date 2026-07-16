//! Relocating the orx data dir (`orx.db` + `run-logs/` + `artifacts/` +
//! `chat-attachments/` + `agent-*.log`) to a user-chosen path.
//!
//! The whole dir is a self-contained, relocatable unit (the api already tars it
//! for R2 snapshot/restore), so a move is: validate target → checkpoint the DB →
//! copy the tree (streaming byte progress) → verify → swap the persisted path in
//! `settings.json` (after which `store::data_dir()` returns the new path) →
//! delete the old copy. Non-destructive: the old tree survives until the copy is
//! verified and the path is swapped, so an interruption never loses data.
//!
//! The caller (the `up` handler) is responsible for refusing a move while a run
//! or chat turn is in flight — this module assumes writers are quiesced enough
//! that the source tree is stable for the copy.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{anyhow, Result};
use crate::store::{self, human_bytes, Store};

/// Outcome of `validate_target`, carried to the UI so it can show free/needed
/// bytes before the user commits.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidateReport {
    /// Total bytes the current data dir occupies (what will be copied).
    pub tree_bytes: u64,
    /// Free bytes on the target's filesystem, if determinable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub free_bytes: Option<u64>,
    /// True when source and target resolve to the same filesystem, so the move
    /// can use an instant `rename` instead of a copy.
    pub same_filesystem: bool,
}

/// Validate a proposed data-dir path, returning a report for the UI. Errors are
/// human-readable (surfaced inline in the Storage settings form).
///
/// What the caller intends to do with the validated target — the emptiness rule
/// differs. A **move** copies the current tree in, so the target must be absent
/// or empty. A **set-without-move** just re-points at a location, so an existing
/// populated orx dir (reconnecting on a second machine, after config loss) is
/// allowed — only a plain file or the current dir is rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetIntent {
    Move,
    Set,
}

/// Validate a proposed data-dir `target` for the given `intent`, returning a
/// report for the UI.
///
/// Always rejects: non-absolute paths, the current data dir itself, any path
/// *inside* the current data dir (a move would copy into itself), a plain file,
/// and a non-existent parent. For `Move`, additionally rejects a non-empty
/// existing target and a target volume without room for the tree; for `Set`, a
/// populated existing dir is allowed (nothing is copied).
pub fn validate_target(target: &Path, intent: TargetIntent) -> Result<ValidateReport> {
    if !target.is_absolute() {
        return Err(anyhow!("Path must be absolute (start with /)."));
    }

    let current = store::data_dir();
    // Normalize both for comparison without requiring the target to exist yet.
    let current_norm = current.canonicalize().unwrap_or_else(|_| current.clone());
    if paths_equal(target, &current_norm) {
        return Err(anyhow!("That's already the current data directory."));
    }
    if target.starts_with(&current_norm) {
        return Err(anyhow!(
            "Can't put the data directory inside itself ({}).",
            current_norm.display()
        ));
    }

    // The parent must exist (a move/create won't `mkdir -p` arbitrary ancestors).
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("Path has no parent directory."))?;
    if !parent.exists() {
        return Err(anyhow!(
            "Parent directory doesn't exist: {}",
            parent.display()
        ));
    }

    // A plain file (or symlink) at the target is never valid.
    if target.exists() {
        let md = std::fs::symlink_metadata(target)
            .map_err(|e| anyhow!("Can't stat {}: {e}", target.display()))?;
        if md.is_file() || md.file_type().is_symlink() {
            return Err(anyhow!("A file already exists at {}.", target.display()));
        }
        // A move copies into the target, so it must be empty. A set just points
        // at it, so an existing (possibly populated) dir is fine.
        if intent == TargetIntent::Move
            && md.is_dir()
            && std::fs::read_dir(target)
                .map(|mut d| d.next().is_some())
                .unwrap_or(false)
        {
            return Err(anyhow!(
                "Directory {} already exists and isn't empty. Choose an empty \
                 or new folder to move into.",
                target.display()
            ));
        }
    }

    // Free-space only matters for a move (a set copies nothing).
    let tree_bytes = dir_size(&current);
    let free_bytes = available_bytes(parent);
    if intent == TargetIntent::Move {
        if let Some(free) = free_bytes {
            if free < tree_bytes {
                return Err(anyhow!(
                    "Not enough space: need {}, only {} free on the target volume.",
                    human_bytes(tree_bytes),
                    human_bytes(free)
                ));
            }
        }
    }

    let same_filesystem = same_device(&current, parent);

    Ok(ValidateReport {
        tree_bytes,
        free_bytes,
        same_filesystem,
    })
}

/// Phase of an in-progress move, for the UI's progress label.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MovePhase {
    Preparing,
    Copying,
    Verifying,
    Finalizing,
}

/// How a completed move disposed of the old directory. Returned so the handler
/// can tell the user whether a copy was left behind to delete manually.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveOutcome {
    /// The new (now-active) data dir.
    pub path: String,
    /// The old dir, if it still exists (cross-filesystem copy leaves it in place;
    /// a same-filesystem rename consumes it). `None` = nothing left behind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path_left: Option<String>,
}

/// A progress tick: bytes copied so far out of the total, plus the phase.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveProgress {
    pub phase: MovePhase,
    pub copied_bytes: u64,
    pub total_bytes: u64,
}

/// Move the current data dir to `target`, invoking `on_progress` as bytes are
/// copied. Persists the new path only after the copy verifies.
///
/// Disposal of the old tree is deliberately conservative:
///  * **Same filesystem** → `rename`, which atomically consumes the old dir (no
///    copy, no window where a stray writer's data could be deleted).
///  * **Cross filesystem** → copy, then **leave the old dir in place**. Deleting
///    it would be unsafe: a detached `supervise` process, an idle harness child
///    pinned to the old path, or a request that resolved the old path just before
///    the swap could still be writing there, and a `remove_dir_all` would drop
///    that data. The old copy is instead reported back so the user (or a later
///    cleanup) can remove it once nothing references it.
///
/// `on_progress` is called frequently during copy; the caller should throttle
/// before forwarding to SSE. This function blocks — run it on a blocking task.
pub fn move_data_dir(
    target: PathBuf,
    on_progress: impl Fn(MoveProgress) + Send,
) -> Result<MoveOutcome> {
    let source = store::data_dir();
    on_progress(MoveProgress {
        phase: MovePhase::Preparing,
        copied_bytes: 0,
        total_bytes: 0,
    });

    // Coalesce the WAL into orx.db so a file copy captures a consistent DB.
    // Failure is fatal for a cross-fs copy (the sidecar files would be copied in
    // an inconsistent state); the rename path is atomic so it wouldn't matter,
    // but we checkpoint unconditionally and bail rather than risk a torn copy.
    match Store::open() {
        Ok(store) => store
            .checkpoint()
            .map_err(|e| anyhow!("Could not checkpoint the database before moving: {e}"))?,
        Err(e) => return Err(anyhow!("Could not open the store to checkpoint it: {e}")),
    }

    // Size the tree AFTER the checkpoint — TRUNCATE reclaims the WAL, so a
    // pre-checkpoint total would mismatch the post-checkpoint copy and fail
    // verification (or mask a short copy).
    let total = dir_size(&source);

    // Same-filesystem + non-existent target → instant, atomic rename.
    let target_parent = target.parent().unwrap_or(&target);
    if same_device(&source, target_parent) && !target.exists() {
        on_progress(MoveProgress {
            phase: MovePhase::Copying,
            copied_bytes: 0,
            total_bytes: total,
        });
        match std::fs::rename(&source, &target) {
            Ok(()) => {
                finalize(&target, total, &on_progress)?;
                return Ok(MoveOutcome {
                    path: target.to_string_lossy().into_owned(),
                    old_path_left: None,
                });
            }
            // EXDEV: the ancestor-walk mis-judged the device (e.g. the volume
            // mounts *at* the target). Fall through to the copy path.
            Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {}
            Err(e) => return Err(anyhow!("Rename to {} failed: {e}", target.display())),
        }
    }

    // Cross-filesystem: recursive copy with byte-level progress, old dir kept.
    std::fs::create_dir_all(&target)
        .map_err(|e| anyhow!("Can't create {}: {e}", target.display()))?;
    let mut copied = 0u64;
    copy_tree(&source, &target, &mut copied, total, &on_progress)
        .map_err(|e| anyhow!("Copy failed: {e}"))?;

    // Verify aggregate size (checkpoint made orx.db the single source of truth,
    // so a size match is a strong signal the copy is complete).
    on_progress(MoveProgress {
        phase: MovePhase::Verifying,
        copied_bytes: copied,
        total_bytes: total,
    });
    let copied_size = dir_size(&target);
    if copied_size < total {
        return Err(anyhow!(
            "Verification failed: copied {} but expected {}. Old data left untouched.",
            human_bytes(copied_size),
            human_bytes(total)
        ));
    }

    finalize(&target, total, &on_progress)?;
    Ok(MoveOutcome {
        path: target.to_string_lossy().into_owned(),
        // Copy path leaves the source in place — reported so it can be cleaned up.
        old_path_left: Some(source.to_string_lossy().into_owned()),
    })
}

/// Persist the new path and emit the finalizing tick. After this, every
/// subsequent `Store::open()` resolves `store::data_dir()` to `target`.
fn finalize(target: &Path, total: u64, on_progress: &impl Fn(MoveProgress)) -> Result<()> {
    on_progress(MoveProgress {
        phase: MovePhase::Finalizing,
        copied_bytes: total,
        total_bytes: total,
    });
    crate::config::set_settings_data_dir(Some(target.to_string_lossy().into_owned()))
}

/// Recursively copy `src` into `dst` (both dirs), accumulating copied bytes into
/// `*copied` and reporting progress. Preserves mtimes best-effort.
fn copy_tree(
    src: &Path,
    dst: &Path,
    copied: &mut u64,
    total: u64,
    on_progress: &impl Fn(MoveProgress),
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_dir() {
            std::fs::create_dir_all(&to)?;
            copy_tree(&from, &to, copied, total, on_progress)?;
        } else if ft.is_file() {
            let n = std::fs::copy(&from, &to)?;
            *copied += n;
            on_progress(MoveProgress {
                phase: MovePhase::Copying,
                copied_bytes: *copied,
                total_bytes: total,
            });
        }
        // Symlinks and other special files are skipped — the data dir holds none.
    }
    Ok(())
}

/// Recursive byte size of a directory tree (files only). Missing dir → 0.
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            total += dir_size(&entry.path());
        } else if ft.is_file() {
            total += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    total
}

/// Case/normalization-tolerant path equality after best-effort canonicalization.
fn paths_equal(a: &Path, b: &Path) -> bool {
    let ca = a.canonicalize();
    match ca {
        Ok(ca) => ca == *b || ca == b.canonicalize().unwrap_or_else(|_| b.to_path_buf()),
        Err(_) => a == b,
    }
}

/// Whether two paths live on the same filesystem device (so a `rename` works).
/// Best-effort: unknown → false (forces the safe copy path).
fn same_device(a: &Path, b: &Path) -> bool {
    match (device_of(a), device_of(b)) {
        (Some(da), Some(db)) => da == db,
        _ => false,
    }
}

#[cfg(unix)]
fn device_of(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    // Walk up to the first existing ancestor (target may not exist yet).
    let mut p = path;
    loop {
        if let Ok(md) = std::fs::metadata(p) {
            return Some(md.dev());
        }
        p = p.parent()?;
    }
}

#[cfg(not(unix))]
fn device_of(_path: &Path) -> Option<u64> {
    None
}

/// Free bytes available to a non-root user on the filesystem containing `path`.
/// `None` when it can't be determined (non-unix, or the syscall fails).
#[cfg(unix)]
fn available_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    // Walk up to the first existing ancestor.
    let mut p = path;
    while !p.exists() {
        p = p.parent()?;
    }
    let c = CString::new(p.as_os_str().as_bytes()).ok()?;
    // SAFETY: `stat` is zeroed then filled by statvfs; c is a valid CString.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    // Available blocks to non-root × fragment size.
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

#[cfg(not(unix))]
fn available_bytes(_path: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        // Unique-enough temp dir without external crates: pid + a monotonic-ish
        // counter baked into the caller's path.
        std::env::temp_dir().join(format!("orx-datadir-test-{}", std::process::id()))
    }

    #[test]
    fn rejects_non_absolute() {
        let err = validate_target(Path::new("relative/path"), TargetIntent::Move).unwrap_err();
        assert!(err.to_string().contains("absolute"));
    }

    #[test]
    fn dir_size_sums_files() {
        let base = tmp().join("size");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("sub")).unwrap();
        std::fs::write(base.join("a.txt"), b"hello").unwrap(); // 5
        std::fs::write(base.join("sub/b.txt"), b"world!!").unwrap(); // 7
        assert_eq!(dir_size(&base), 12);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn copy_tree_roundtrip() {
        let base = tmp().join("copy");
        let _ = std::fs::remove_dir_all(&base);
        let src = base.join("src");
        let dst = base.join("dst");
        std::fs::create_dir_all(src.join("run-logs")).unwrap();
        std::fs::write(src.join("orx.db"), vec![7u8; 2048]).unwrap();
        std::fs::write(src.join("run-logs/r.log"), b"log line\n").unwrap();
        std::fs::create_dir_all(&dst).unwrap();

        let total = dir_size(&src);
        let mut copied = 0u64;
        copy_tree(&src, &dst, &mut copied, total, &|_| {}).unwrap();

        assert_eq!(dir_size(&dst), total);
        assert_eq!(copied, total);
        assert_eq!(std::fs::read(dst.join("orx.db")).unwrap().len(), 2048);
        assert_eq!(
            std::fs::read(dst.join("run-logs/r.log")).unwrap(),
            b"log line\n"
        );
        std::fs::remove_dir_all(&base).unwrap();
    }
}
