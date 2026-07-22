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

/// Root for per-chat-session worktrees of a repo's hub clone
/// (`~/.cache/openresearch/worktrees/<owner>/<repo>/<session-id>`). Kept
/// outside `repos/` so a worktree can never collide with a real repo name.
pub fn worktrees_root(owner: &str, repo: &str) -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cache")
        .join("openresearch")
        .join("worktrees")
        .join(owner)
        .join(repo)
}

pub fn session_worktree_path(owner: &str, repo: &str, session_id: &str) -> PathBuf {
    worktrees_root(owner, repo).join(session_id)
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

/// `GITHUB_TOKEN` env, else the synced env file (UI-pasted token), else
/// `gh auth token`, else None (public-repo fallback).
pub fn resolve_github_token() -> Option<String> {
    if let Ok(t) = std::env::var("GITHUB_TOKEN") {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Some(t);
        }
    }
    if let Some(t) = crate::config::synced_env_var("GITHUB_TOKEN") {
        return Some(t);
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

/// Ensure a private worktree of the hub clone for one chat session, so
/// parallel agents on the same project never share (or stomp) a checkout.
/// Worktrees share the hub's object store and refs: a branch created in one
/// is immediately visible in all, one `fetch` updates everyone, and git
/// refuses to check out a branch that another worktree already holds.
///
/// The worktree starts **detached** on the baseline tip — checking out the
/// baseline branch itself would claim it and block every sibling; the agent
/// checks out its own experiment branch from there.
pub fn ensure_session_worktree(
    owner: &str,
    repo: &str,
    baseline_branch: &str,
    session_id: &str,
) -> Result<PathBuf> {
    let hub = ensure_clone(owner, repo, baseline_branch)?;
    let dir = session_worktree_path(owner, repo, session_id);
    if dir.join(".git").exists() {
        // `.git` is a gitdir-pointer file in a worktree. Validate it — a wiped
        // hub clone (cache cleared, then re-cloned by ensure_clone above)
        // orphans old worktrees, which must be rebuilt, not returned.
        if git(Some(&dir), &["rev-parse", "--is-inside-work-tree"]).is_ok() {
            return Ok(dir);
        }
        std::fs::remove_dir_all(&dir)
            .map_err(|e| anyhow!("Could not remove stale worktree {}: {}", dir.display(), e))?;
    }
    // A manually deleted worktree dir leaves a stale registration behind that
    // would make `worktree add` at the same path fail.
    let _ = git(Some(&hub), &["worktree", "prune"]);
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow!("Could not create {}: {}", parent.display(), e))?;
    }
    let target = dir.to_string_lossy().to_string();
    let base = format!("refs/remotes/origin/{baseline_branch}");
    git(Some(&hub), &["worktree", "add", "--detach", &target, &base])?;
    Ok(dir)
}

/// Remove a session's worktree (on session/project delete). Uncommitted
/// scratch is discarded deliberately — real work is committed and pushed per
/// the playbook contract. Best-effort: cleanup must never block the delete.
pub fn remove_session_worktree(owner: &str, repo: &str, session_id: &str) {
    let dir = session_worktree_path(owner, repo, session_id);
    if !dir.exists() {
        return;
    }
    let hub = clone_path(owner, repo);
    if hub.join(".git").is_dir() {
        let _ = git(
            Some(&hub),
            &["worktree", "remove", "--force", &dir.to_string_lossy()],
        );
        let _ = git(Some(&hub), &["worktree", "prune"]);
    }
    // Hub gone (cache wiped) or `worktree remove` refused: take the dir anyway.
    let _ = std::fs::remove_dir_all(&dir);
}

/// Seed a fresh (empty) GitHub repo from the tip of another repo — the
/// fork-by-copy the platform does on import. Shallow-clones the source
/// (`src_branch`, or its default branch), re-roots the snapshot as a single
/// orphan commit (a shallow tip's parents aren't in the clone, so pushing it
/// as-is would be rejected), and pushes it as the new repo's `main`.
pub fn seed_copy(
    src_owner: &str,
    src_repo: &str,
    src_branch: Option<&str>,
    dst_owner: &str,
    dst_repo: &str,
) -> Result<()> {
    let tmp = std::env::temp_dir().join(format!("orx-seed-{}", uuid::Uuid::new_v4()));
    let result = seed_copy_in(&tmp, src_owner, src_repo, src_branch, dst_owner, dst_repo);
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

fn seed_copy_in(
    dir: &Path,
    src_owner: &str,
    src_repo: &str,
    src_branch: Option<&str>,
    dst_owner: &str,
    dst_repo: &str,
) -> Result<()> {
    let target = dir.to_string_lossy().to_string();
    let mut args = vec!["clone", "--depth=1", "--single-branch"];
    if let Some(branch) = src_branch {
        args.extend(["--branch", branch]);
    }
    // ssh first, https fallback — same auth order as ensure_clone.
    let ssh = format!("git@github.com:{src_owner}/{src_repo}.git");
    let https = format!("https://github.com/{src_owner}/{src_repo}.git");
    let mut ssh_args = args.clone();
    ssh_args.extend([ssh.as_str(), target.as_str()]);
    if git(None, &ssh_args).is_err() {
        let mut https_args = args;
        https_args.extend([https.as_str(), target.as_str()]);
        if let Err(err) = git(None, &https_args) {
            return Err(anyhow!(
                "Could not clone {src_owner}/{src_repo} (tried ssh and https): {err}"
            ));
        }
    }
    git(Some(dir), &["checkout", "--orphan", "orx-seed"])?;
    git(Some(dir), &["add", "-A"])?;
    // An empty source stages nothing; seed the stub a blank project gets.
    if git(Some(dir), &["status", "--porcelain"])?.is_empty() {
        std::fs::write(dir.join("README.md"), format!("# {dst_repo}\n"))
            .map_err(|e| anyhow!("Could not write README.md: {}", e))?;
        git(Some(dir), &["add", "-A"])?;
    }
    git(
        Some(dir),
        &[
            "-c",
            "user.name=orx",
            "-c",
            "user.email=orx@openresearch.sh",
            "commit",
            "-m",
            &format!("orx: import {src_owner}/{src_repo}"),
        ],
    )?;
    let dst_ssh = format!("git@github.com:{dst_owner}/{dst_repo}.git");
    let dst_https = format!("https://github.com/{dst_owner}/{dst_repo}.git");
    if git(Some(dir), &["push", &dst_ssh, "HEAD:main"]).is_err() {
        git(Some(dir), &["push", &dst_https, "HEAD:main"])?;
    }
    Ok(())
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
    git(
        Some(repo_path),
        &["branch", "--no-track", "-f", new_branch, &base],
    )?;
    if let Err(err) = git(Some(repo_path), &["push", "-u", "origin", new_branch]) {
        // Leave nothing behind — a retry re-picks the same slug.
        let _ = git(Some(repo_path), &["branch", "-D", new_branch]);
        return Err(err);
    }
    Ok(())
}

/// Head SHA of a branch — the *remote* tip when it exists (that's what a job
/// clones), the local ref otherwise. The opposite preference of
/// `resolve_branch_commit`, which serves the code browser and wants the
/// agent's not-yet-pushed local work.
pub fn branch_head_sha(repo_path: &Path, branch: &str) -> Result<String> {
    let remote = format!("refs/remotes/origin/{branch}");
    if let Ok(sha) = git(Some(repo_path), &["rev-parse", &remote]) {
        return Ok(sha);
    }
    git(Some(repo_path), &["rev-parse", branch])
}

/// Whether origin already has the branch (a cheap network check).
pub fn branch_on_remote(repo_path: &Path, branch: &str) -> Result<bool> {
    let out = git(Some(repo_path), &["ls-remote", "--heads", "origin", branch])?;
    Ok(!out.is_empty())
}

/// A file's content at a specific commit (`git show <sha>:<path>`), i.e.
/// exactly what a job cloning that sha will see — not the working tree.
pub fn file_at(repo_path: &Path, sha: &str, path: &str) -> Result<String> {
    git(Some(repo_path), &["show", &format!("{sha}:{path}")])
}

/// Whether the repo tracks `path` (local check, no network).
pub fn is_tracked(repo_path: &Path, path: &str) -> bool {
    git(
        Some(repo_path),
        &["ls-files", "--error-unmatch", "--", path],
    )
    .is_ok()
}

pub fn push_branch(repo_path: &Path, branch: &str) -> Result<()> {
    git(Some(repo_path), &["push", "-u", "origin", branch])?;
    Ok(())
}

// --- diffs ------------------------------------------------------------------

/// Whole-diff cap, mirroring the OpenResearch api's MAX_DIFF_BYTES.
pub const MAX_DIFF_BYTES: usize = 2 * 1024 * 1024;

pub struct DiffPayload {
    pub diff: String,
    pub truncated: bool,
    pub bytes_read: usize,
}

pub struct CommitInfo {
    pub sha: String,
    pub subject: String,
    /// Unix seconds.
    pub committed_at: i64,
}

/// Like `git` but raw stdout bytes, no trim, and extra tolerated exit codes
/// (`git diff --no-index` exits 1 when the files differ).
fn git_bytes(dir: &Path, args: &[&str], ok_codes: &[i32]) -> Result<Vec<u8>> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    let out = cmd
        .args(args)
        .output()
        .map_err(|e| anyhow!("Could not run git: {}", e))?;
    let code = out.status.code().unwrap_or(-1);
    if !out.status.success() && !ok_codes.contains(&code) {
        return Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

fn cap_diff(mut bytes: Vec<u8>) -> DiffPayload {
    let truncated = bytes.len() > MAX_DIFF_BYTES;
    if truncated {
        bytes.truncate(MAX_DIFF_BYTES);
    }
    let bytes_read = bytes.len();
    // lossy: the cap can land mid multibyte char
    DiffPayload {
        diff: String::from_utf8_lossy(&bytes).into_owned(),
        truncated,
        bytes_read,
    }
}

/// Resolve a branch name or sha to something git can diff. Prefers the local
/// ref (where the agent works) and falls back to origin.
fn resolve_commitish(repo: &Path, name: &str) -> Result<String> {
    for cand in [name.to_string(), format!("refs/remotes/origin/{name}")] {
        let probe = format!("{cand}^{{commit}}");
        if git(Some(repo), &["rev-parse", "--verify", "--quiet", &probe]).is_ok() {
            return Ok(cand);
        }
    }
    Err(anyhow!("unknown git ref: {name}"))
}

/// Cumulative diff `base...head` (merge-base semantics, same as the cloud
/// compare endpoint).
pub fn diff_range(repo: &Path, base: &str, head: &str) -> Result<DiffPayload> {
    let base = resolve_commitish(repo, base)?;
    let head = resolve_commitish(repo, head)?;
    let range = format!("{base}...{head}");
    Ok(cap_diff(git_bytes(
        repo,
        &["--no-pager", "diff", &range],
        &[],
    )?))
}

/// Single-commit diff. `git show` handles root commits, unlike `sha~1..sha`.
pub fn commit_diff(repo: &Path, sha: &str) -> Result<DiffPayload> {
    Ok(cap_diff(git_bytes(
        repo,
        &["--no-pager", "show", "--format=", "--patch", sha],
        &[],
    )?))
}

fn parse_commit_lines(out: &str) -> Vec<CommitInfo> {
    out.lines()
        .filter_map(|line| {
            let mut parts = line.split('\u{1f}');
            Some(CommitInfo {
                sha: parts.next()?.to_string(),
                subject: parts.next()?.to_string(),
                committed_at: parts.next()?.parse().ok()?,
            })
        })
        .collect()
}

/// Commits on `head` that aren't on `base`, newest first.
pub fn list_commits_between(
    repo: &Path,
    base: &str,
    head: &str,
    limit: usize,
) -> Result<Vec<CommitInfo>> {
    let base = resolve_commitish(repo, base)?;
    let head = resolve_commitish(repo, head)?;
    let range = format!("{base}..{head}");
    let out = git(
        Some(repo),
        &[
            "log",
            "--format=%H%x1f%s%x1f%ct",
            "-n",
            &limit.to_string(),
            &range,
        ],
    )?;
    Ok(parse_commit_lines(&out))
}

/// Latest commits on a branch, newest first.
pub fn list_commits(repo: &Path, branch: &str, limit: usize) -> Result<Vec<CommitInfo>> {
    let branch = resolve_commitish(repo, branch)?;
    let out = git(
        Some(repo),
        &[
            "log",
            "--format=%H%x1f%s%x1f%ct",
            "-n",
            &limit.to_string(),
            &branch,
        ],
    )?;
    Ok(parse_commit_lines(&out))
}

/// Uncommitted changes in the clone: tracked edits vs HEAD plus untracked
/// files rendered as new-file diffs. Returns (current branch, diff).
pub fn working_tree_diff(repo: &Path) -> Result<(Option<String>, DiffPayload)> {
    let branch = current_branch(repo);
    let payload = working_tree_diff_against(repo, None)?;
    Ok((branch, payload))
}

/// The working tree diffed against `base` (a merge-base sha for a session
/// worktree) instead of `HEAD` — the agent commits its work to experiment
/// branches, so a bare `HEAD` diff would hide everything it committed since
/// forking from the baseline. `None` reproduces `working_tree_diff`'s `HEAD`
/// behaviour, keeping the clone-scoped endpoint untouched. Tracked edits come
/// from one `git diff`; untracked files are appended as new-file diffs, both
/// under the shared `MAX_DIFF_BYTES` cap.
pub fn working_tree_diff_against(repo: &Path, base: Option<&str>) -> Result<DiffPayload> {
    let base = base.unwrap_or("HEAD");
    let mut bytes = git_bytes(repo, &["--no-pager", "diff", base], &[1])?;
    let untracked = git(Some(repo), &["ls-files", "--others", "--exclude-standard"])?;
    for f in untracked.lines().filter(|l| !l.is_empty()) {
        if bytes.len() > MAX_DIFF_BYTES {
            break;
        }
        if let Ok(chunk) = git_bytes(
            repo,
            &["--no-pager", "diff", "--no-index", "--", "/dev/null", f],
            &[1],
        ) {
            bytes.extend_from_slice(&chunk);
        }
    }
    Ok(cap_diff(bytes))
}

/// The merge-base of `a` and `b` (`git merge-base`), or `Ok(None)` when git
/// can't compute one — an unresolvable ref, unrelated histories, or a fresh
/// worktree whose baseline ref is momentarily missing. Callers treat `None` as
/// "fall back to HEAD" rather than an error: a missing merge-base is a routine
/// state, not a failure.
pub fn merge_base(repo: &Path, a: &str, b: &str) -> Result<Option<String>> {
    match git(Some(repo), &["merge-base", a, b]) {
        Ok(sha) if !sha.is_empty() => Ok(Some(sha)),
        _ => Ok(None),
    }
}

/// How a changed file differs from the diff base. Serialized lowercase to
/// match the single-letter status badges the UI renders; `renamed` and
/// `untracked` have no git single-letter porcelain equivalent (rename is `R`
/// with a score, untracked comes from a separate `ls-files` pass).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangedStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Untracked,
}

/// One entry in the session worktree's change list. `old_path` is only set for
/// renames (the pre-rename path); the list is complete even when the unified
/// diff truncates, because it comes from a separate name-status pass.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangedFile {
    pub path: String,
    pub status: ChangedStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
}

/// The set of files that differ between `base` and the worktree — tracked
/// changes from `git diff --name-status -z` (so rename pairs and non-ASCII
/// paths survive intact) plus untracked-not-ignored files from `git ls-files
/// --others`. This is the authoritative change list the Changes view renders:
/// it stays complete even when the unified diff hits `MAX_DIFF_BYTES`. Sorted
/// by path for a stable ordering.
pub fn changed_files(repo: &Path, base: &str) -> Result<Vec<ChangedFile>> {
    let bytes = git_bytes(
        repo,
        &["--no-pager", "diff", "--name-status", "-z", base],
        &[1],
    )?;
    // -z output is NUL-delimited fields (not lines): a status field followed by
    // one path, except renames/copies which emit `R<score>`/`C<score>` and two
    // paths (old, new). Walk the fields with an explicit cursor so the extra
    // rename path is consumed in lockstep.
    let fields = split_nul(&bytes);
    let mut files = Vec::new();
    let mut i = 0;
    while i < fields.len() {
        let code = &fields[i];
        let first = code.chars().next().unwrap_or(' ');
        match first {
            'R' | 'C' => {
                // Rename/copy: <old> then <new>. A copy leaves the original in
                // place, so surface only the new path (as a rename) — the old
                // file is unchanged and needs no row.
                let old = fields.get(i + 1).cloned();
                let new = fields.get(i + 2).cloned();
                if let Some(path) = new {
                    files.push(ChangedFile {
                        path,
                        status: ChangedStatus::Renamed,
                        old_path: old,
                    });
                }
                i += 3;
            }
            _ => {
                if let Some(path) = fields.get(i + 1).cloned() {
                    let status = match first {
                        'A' => ChangedStatus::Added,
                        'D' => ChangedStatus::Deleted,
                        _ => ChangedStatus::Modified,
                    };
                    files.push(ChangedFile {
                        path,
                        status,
                        old_path: None,
                    });
                }
                i += 2;
            }
        }
    }
    let untracked = git_bytes(
        repo,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        &[],
    )?;
    for path in split_nul(&untracked) {
        // `--others` reports a nested git repo as `dir/` — not a file; drop it.
        if path.ends_with('/') {
            continue;
        }
        files.push(ChangedFile {
            path,
            status: ChangedStatus::Untracked,
            old_path: None,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

/// The checked-out branch name, or `None` when detached (`rev-parse
/// --abbrev-ref HEAD` prints the literal `HEAD` — e.g. a fresh worktree
/// before the agent checks out its branch) or when rev-parse fails outright
/// (unborn HEAD in an empty repo). Never errors — no branch is an answer.
pub fn current_branch(repo: &Path) -> Option<String> {
    git(Some(repo), &["rev-parse", "--abbrev-ref", "HEAD"])
        .ok()
        .filter(|b| b != "HEAD" && !b.is_empty())
}

/// NUL-separated git output (`-z` flags) → lossy-decoded strings, empties
/// dropped.
fn split_nul(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

/// Every path in the checkout that git would show as tracked or
/// untracked-but-not-ignored (`git ls-files --cached --others
/// --exclude-standard -z`). NUL-separated so non-ASCII paths aren't quoted;
/// gitignored trees (`target/`, `node_modules/`, `.git/`) drop out for free.
/// Repo-relative, unsorted.
pub fn list_worktree_files(repo: &Path) -> Result<Vec<String>> {
    let bytes = git_bytes(
        repo,
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ],
        &[],
    )?;
    let mut entries = split_nul(&bytes);
    // `--others` reports a nested git repo as `dir/` — a directory, nothing
    // servable as a file; drop those here so every client benefits.
    entries.retain(|e| !e.ends_with('/'));
    Ok(entries)
}

/// Resolve a branch name to its commit sha — the *local* ref first, then
/// origin's: the code browser wants the agent's latest work, which lives
/// locally before any push (the opposite preference of `branch_head_sha`,
/// which serves jobs that clone from the remote; `resolve_commitish` is the
/// diff-side sibling that also accepts raw shas). `Ok(None)` when neither
/// exists. Only real branch names are accepted: rev-suffix expressions
/// (`@{...}`, `^`, `~`, `:`, whitespace) are rejected up front, and the
/// leading-`-` check is belt-and-braces — the `refs/heads/` prefix already
/// keeps the name out of option position.
pub fn resolve_branch_commit(repo: &Path, name: &str) -> Result<Option<String>> {
    let suspicious = name.is_empty()
        || name.starts_with('-')
        || name.contains("@{")
        || name.contains(['^', '~', ':'])
        || name.chars().any(char::is_whitespace);
    if suspicious {
        return Ok(None);
    }
    for prefix in ["refs/heads/", "refs/remotes/origin/"] {
        let full = format!("{prefix}{name}");
        if let Ok(sha) = git(Some(repo), &["rev-parse", "--verify", "--quiet", &full]) {
            if !sha.is_empty() {
                return Ok(Some(sha));
            }
        }
    }
    Ok(None)
}

/// Every path in the tree of a commit (`git ls-tree -r -z --name-only`) —
/// the committed state, independent of any checkout. Repo-relative, unsorted.
pub fn list_tree_files(repo: &Path, sha: &str) -> Result<Vec<String>> {
    let bytes = git_bytes(
        repo,
        &["ls-tree", "-r", "-z", "--name-only", sha, "--"],
        &[],
    )?;
    Ok(split_nul(&bytes))
}

/// A file's committed content at `<sha>:<path>`, read from a streamed
/// `git cat-file blob` and capped at `limit` bytes — a multi-GB committed
/// blob costs one pipe buffer, not one allocation (unlike `file_at`, which
/// is fine for its known-small callers). Existence is checked first with
/// `cat-file -e` (exit code only, no error-message parsing): `Ok(None)`
/// when the path isn't in that tree. Returns `(content, truncated)`,
/// lossy-decoded, byte-exact up to the cap — no trimming.
pub fn file_at_capped(
    repo: &Path,
    sha: &str,
    path: &str,
    limit: u64,
) -> Result<Option<(String, bool)>> {
    use std::process::Stdio;
    let spec = format!("{sha}:{path}");
    let exists = Command::new("git")
        .current_dir(repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(["cat-file", "-e", &spec])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| anyhow!("Could not run git: {}", e))?;
    if !exists.success() {
        return Ok(None);
    }
    let mut child = Command::new("git")
        .current_dir(repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(["cat-file", "blob", &spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| anyhow!("Could not run git: {}", e))?;
    let mut buf = Vec::new();
    let read = {
        use std::io::Read as _;
        let stdout = child.stdout.take().expect("stdout was piped");
        stdout.take(limit + 1).read_to_end(&mut buf)
    };
    let truncated = buf.len() as u64 > limit;
    // Reap the child before propagating any read error — no zombies. Kill
    // only when it may still be streaming (read error, or we stopped at the
    // cap): after a complete read EOF means git closed stdout and exits on
    // its own, and killing it then could race its natural exit into a bogus
    // signal-death status.
    if read.is_err() || truncated {
        let _ = child.kill();
    }
    let status = child.wait();
    read.map_err(|e| anyhow!("read failed: {}", e))?;
    if !truncated {
        // A cat-file failure after the `-e` probe (the path names a tree via
        // a crafted request, or the object vanished) must not masquerade as
        // an empty file. When truncated we killed it — any status goes.
        let status = status.map_err(|e| anyhow!("git cat-file blob: {}", e))?;
        if !status.success() {
            return Err(anyhow!("git cat-file blob {spec} failed"));
        }
    }
    buf.truncate(limit as usize);
    Ok(Some((
        String::from_utf8_lossy(&buf).into_owned(),
        truncated,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway git repo under the temp dir with one seed commit on `main`.
    /// Configured with a fixed identity and no signing/hooks so `commit`
    /// succeeds regardless of the host's global git config.
    fn temp_repo() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("orx-git-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create repo dir");
        run(&dir, &["init", "-q", "-b", "main"]);
        run(&dir, &["config", "user.name", "orx-test"]);
        run(&dir, &["config", "user.email", "orx-test@example.com"]);
        run(&dir, &["config", "commit.gpgsign", "false"]);
        write(&dir, "seed.txt", "seed\n");
        run(&dir, &["add", "-A"]);
        run(&dir, &["commit", "-q", "-m", "seed"]);
        dir
    }

    fn run(dir: &Path, args: &[&str]) -> String {
        git(Some(dir), args).unwrap_or_else(|e| panic!("git {args:?} failed: {e}"))
    }

    fn write(dir: &Path, rel: &str, contents: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, contents).expect("write file");
    }

    fn statuses(files: &[ChangedFile]) -> Vec<(String, ChangedStatus)> {
        files
            .iter()
            .map(|f| (f.path.clone(), f.status.clone()))
            .collect()
    }

    #[test]
    fn merge_base_normal_and_missing() {
        let repo = temp_repo();
        let seed = run(&repo, &["rev-parse", "HEAD"]);
        // A second commit on main: merge-base(HEAD, seed) is the seed itself.
        write(&repo, "a.txt", "a\n");
        run(&repo, &["add", "-A"]);
        run(&repo, &["commit", "-q", "-m", "second"]);
        assert_eq!(merge_base(&repo, "HEAD", &seed).unwrap(), Some(seed));
        // An unknown ref yields None, never an error.
        assert_eq!(merge_base(&repo, "HEAD", "no-such-ref").unwrap(), None);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn changed_files_committed_ahead_uncommitted_untracked() {
        let repo = temp_repo();
        let base = run(&repo, &["rev-parse", "HEAD"]);
        // Committed-ahead: a new file committed on top of the base.
        write(&repo, "committed.txt", "c\n");
        run(&repo, &["add", "-A"]);
        run(&repo, &["commit", "-q", "-m", "add committed"]);
        // Uncommitted: edit a tracked file in the working tree.
        write(&repo, "seed.txt", "seed edited\n");
        // Untracked: a brand-new unstaged file.
        write(&repo, "untracked.txt", "u\n");

        let got = statuses(&changed_files(&repo, &base).unwrap());
        assert_eq!(
            got,
            vec![
                ("committed.txt".to_string(), ChangedStatus::Added),
                ("seed.txt".to_string(), ChangedStatus::Modified),
                ("untracked.txt".to_string(), ChangedStatus::Untracked),
            ]
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn changed_files_rename_and_delete() {
        let repo = temp_repo();
        write(
            &repo,
            "old.txt",
            "the quick brown fox jumps over the lazy dog\n",
        );
        run(&repo, &["add", "-A"]);
        run(&repo, &["commit", "-q", "-m", "add old"]);
        write(&repo, "gone.txt", "delete me\n");
        run(&repo, &["add", "-A"]);
        run(&repo, &["commit", "-q", "-m", "add gone"]);
        let base = run(&repo, &["rev-parse", "HEAD"]);
        // Rename old.txt -> new.txt (identical content: a pure rename) and
        // delete gone.txt, then commit so they land in the name-status diff.
        run(&repo, &["mv", "old.txt", "new.txt"]);
        run(&repo, &["rm", "-q", "gone.txt"]);
        run(&repo, &["commit", "-q", "-m", "rename and delete"]);

        let files = changed_files(&repo, &base).unwrap();
        let renamed = files
            .iter()
            .find(|f| f.path == "new.txt")
            .expect("rename row");
        assert_eq!(renamed.status, ChangedStatus::Renamed);
        assert_eq!(renamed.old_path.as_deref(), Some("old.txt"));
        let deleted = files
            .iter()
            .find(|f| f.path == "gone.txt")
            .expect("delete row");
        assert_eq!(deleted.status, ChangedStatus::Deleted);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn working_tree_diff_against_includes_committed_ahead() {
        let repo = temp_repo();
        let base = run(&repo, &["rev-parse", "HEAD"]);
        // Content committed after the base must appear in the diff — a bare HEAD
        // diff (base = None) would show nothing, since the tree is clean.
        write(&repo, "ahead.txt", "committed line\n");
        run(&repo, &["add", "-A"]);
        run(&repo, &["commit", "-q", "-m", "ahead"]);

        assert!(working_tree_diff_against(&repo, None)
            .unwrap()
            .diff
            .is_empty());
        let payload = working_tree_diff_against(&repo, Some(&base)).unwrap();
        assert!(payload.diff.contains("ahead.txt"));
        assert!(payload.diff.contains("committed line"));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn detached_zero_change_is_empty() {
        // A fresh detached checkout at the baseline tip with no edits: base and
        // HEAD coincide, so there are no changed files and an empty diff — the
        // "detached, No changes yet" state the UI renders.
        let repo = temp_repo();
        let head = run(&repo, &["rev-parse", "HEAD"]);
        run(&repo, &["checkout", "-q", "--detach", &head]);
        let base = merge_base(&repo, "main", "HEAD")
            .unwrap()
            .expect("merge-base");
        assert!(changed_files(&repo, &base).unwrap().is_empty());
        assert!(working_tree_diff_against(&repo, Some(&base))
            .unwrap()
            .diff
            .is_empty());
        assert_eq!(current_branch(&repo), None);
        let _ = std::fs::remove_dir_all(&repo);
    }
}
