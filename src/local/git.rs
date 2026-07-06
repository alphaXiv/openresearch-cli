//! Git operations for local mode — shell out to the `git` binary (already a
//! hard dependency of the workflow; no libgit2). Clones live at
//! `~/.cache/openresearch/repos/<owner>/<repo>`, the same convention SKILL.md
//! documents for manual diffing.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{anyhow, Result};

pub fn clones_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cache")
        .join("openresearch")
        .join("repos")
}

pub fn clone_path(owner: &str, repo: &str) -> PathBuf {
    clones_root().join(owner).join(repo)
}

/// Run git with `args`, returning trimmed stdout; failures carry git's stderr.
/// Headless: git must fail fast rather than prompt on /dev/tty (these calls
/// run under a server, where a prompt would hang a worker forever).
fn git(dir: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    if std::env::var_os("GIT_SSH_COMMAND").is_none() && std::env::var_os("GIT_SSH").is_none() {
        cmd.env("GIT_SSH_COMMAND", "ssh -oBatchMode=yes");
    }
    let out = cmd
        .args(args)
        .output()
        .map_err(|e| anyhow!("Could not run git: {}", e))?;
    if !out.status.success() {
        return Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `GITHUB_TOKEN` env, else `gh auth token`, else None (public-repo fallback).
pub fn resolve_github_token() -> Option<String> {
    if let Ok(t) = std::env::var("GITHUB_TOKEN") {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Some(t);
        }
    }
    let out = Command::new("gh").args(["auth", "token"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!t.is_empty()).then_some(t)
}

/// Fail early on a typo'd baseline branch — otherwise it only surfaces much
/// later as an opaque `git push` refspec error on the first run.
fn assert_branch_exists(dir: &Path, owner: &str, repo: &str, branch: &str) -> Result<()> {
    let remote = format!("refs/remotes/origin/{branch}");
    if git(Some(dir), &["rev-parse", "--verify", "--quiet", &remote]).is_err() {
        return Err(anyhow!(
            "Branch '{branch}' not found in {owner}/{repo} — check the project's baseline branch."
        ));
    }
    Ok(())
}

/// Clone `owner/repo` into the cache (ssh first, then https) or, when the
/// clone already exists, fetch. Validates that `baseline_branch` exists on
/// the remote. Returns the clone path.
pub fn ensure_clone(owner: &str, repo: &str, baseline_branch: &str) -> Result<PathBuf> {
    let dir = clone_path(owner, repo);
    if dir.join(".git").is_dir() {
        git(Some(&dir), &["fetch", "origin"])?;
        assert_branch_exists(&dir, owner, repo, baseline_branch)?;
        return Ok(dir);
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow!("Could not create {}: {}", parent.display(), e))?;
    }
    let target = dir.to_string_lossy().to_string();
    // Test seam: ORX_GIT_REMOTE_BASE=file:///some/root clones <base>/<owner>/<repo>.
    if let Ok(base) = std::env::var("ORX_GIT_REMOTE_BASE") {
        let url = format!("{}/{owner}/{repo}", base.trim_end_matches('/'));
        git(None, &["clone", &url, &target])?;
        assert_branch_exists(&dir, owner, repo, baseline_branch)?;
        return Ok(dir);
    }
    let ssh = format!("git@github.com:{owner}/{repo}.git");
    let https = format!("https://github.com/{owner}/{repo}.git");
    // ssh covers private repos with keys; https covers public repos and
    // credential-helper setups. Surface the https error (the common path).
    if git(None, &["clone", &ssh, &target]).is_err() {
        if let Err(err) = git(None, &["clone", &https, &target]) {
            return Err(anyhow!(
                "Could not clone {owner}/{repo} (tried ssh and https): {err}"
            ));
        }
    }
    assert_branch_exists(&dir, owner, repo, baseline_branch)?;
    Ok(dir)
}

/// Create `new_branch` from `parent_branch`'s tip and push it to origin —
/// the branch must exist on GitHub before an HF job can clone it.
pub fn create_experiment_branch(
    repo_path: &Path,
    parent_branch: &str,
    new_branch: &str,
) -> Result<()> {
    git(Some(repo_path), &["fetch", "origin"])?;
    // Prefer the remote tip; a never-pushed parent falls back to the local ref.
    let remote_parent = format!("refs/remotes/origin/{parent_branch}");
    let base = if git(Some(repo_path), &["rev-parse", "--verify", &remote_parent]).is_ok() {
        remote_parent
    } else {
        parent_branch.to_string()
    };
    // -f: a stale local branch is residue from an earlier failed attempt (a
    // live branch would have an experiment row and its slug never re-picked).
    git(Some(repo_path), &["branch", "--no-track", "-f", new_branch, &base])?;
    if let Err(err) = git(Some(repo_path), &["push", "-u", "origin", new_branch]) {
        // Leave nothing behind — a retry re-picks the same slug.
        let _ = git(Some(repo_path), &["branch", "-D", new_branch]);
        return Err(err);
    }
    Ok(())
}

/// Head SHA of a branch — the remote tip when it exists (that's what a job
/// clones), the local ref otherwise.
pub fn branch_head_sha(repo_path: &Path, branch: &str) -> Result<String> {
    let remote = format!("refs/remotes/origin/{branch}");
    if let Ok(sha) = git(Some(repo_path), &["rev-parse", &remote]) {
        return Ok(sha);
    }
    git(Some(repo_path), &["rev-parse", branch])
}

/// Whether origin already has the branch (a cheap network check).
pub fn branch_on_remote(repo_path: &Path, branch: &str) -> Result<bool> {
    let out = git(
        Some(repo_path),
        &["ls-remote", "--heads", "origin", branch],
    )?;
    Ok(!out.is_empty())
}

/// Whether the repo tracks `path` (local check, no network).
pub fn is_tracked(repo_path: &Path, path: &str) -> bool {
    git(Some(repo_path), &["ls-files", "--error-unmatch", "--", path]).is_ok()
}

pub fn push_branch(repo_path: &Path, branch: &str) -> Result<()> {
    git(Some(repo_path), &["push", "-u", "origin", branch])?;
    Ok(())
}
