//! Local project creation — clone the repo and insert the project row. Used by
//! the `orx up` HTTP API (`POST /api/projects`).
//!
//! The project starts with an empty experiment tree. The first experiment
//! created without a parent via `orx create-experiment` becomes the baseline
//! root — the control every variant is measured against.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::store::{now_ms, Store};

use super::model::LocalProject;
use super::{git, slugify};

fn unique_project_slug(store: &Store, base: &str) -> Result<String> {
    let taken: HashSet<String> = store
        .list_local_projects()?
        .into_iter()
        .map(|p| p.slug)
        .collect();
    if !taken.contains(base) {
        return Ok(base.to_string());
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}-{n}");
        if !taken.contains(&candidate) {
            return Ok(candidate);
        }
        n += 1;
    }
}

pub(crate) fn expand_path(path: &str) -> Result<PathBuf> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(crate::error::anyhow!("project path is required"));
    }
    if trimmed == "~" || trimmed.starts_with("~/") {
        let home = dirs::home_dir()
            .ok_or_else(|| crate::error::anyhow!("Could not resolve the home directory"))?;
        return Ok(if trimmed == "~" {
            home
        } else {
            home.join(&trimmed[2..])
        });
    }
    let path = PathBuf::from(trimmed);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn prepare_path(
    path: &str,
    create_folder: bool,
    initialize_git: bool,
    clone_url: Option<&str>,
) -> Result<PathBuf> {
    let path = expand_path(path)?;
    if let Some(url) = clone_url.map(str::trim).filter(|url| !url.is_empty()) {
        if path.exists() {
            let mut entries = std::fs::read_dir(&path)?;
            if entries.next().is_some() {
                return Err(crate::error::anyhow!(
                    "{} must be empty before cloning the paper repository",
                    path.display()
                ));
            }
        } else if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        git::clone_public(url, &path)?;
        git::rename_origin_to_upstream(&path)?;
    } else if !path.exists() {
        if !create_folder {
            return Err(crate::error::anyhow!(
                "{} does not exist; choose an existing folder or allow OpenResearch to create it",
                path.display()
            ));
        }
        std::fs::create_dir_all(&path)?;
    } else if !path.is_dir() {
        return Err(crate::error::anyhow!("{} is not a folder", path.display()));
    }

    if !git::is_repository(&path) {
        if !initialize_git {
            return Err(crate::error::anyhow!(
                "Experiments need a local Git repository. Confirm initialization for {} and try again.",
                path.display()
            ));
        }
        git::initialize_repository(&path)?;
    }
    let root = git::repository_root(&path)?;
    git::validate_project_repository(&root)?;
    Ok(root)
}

/// Register a local folder as a project. No experiments are created —
/// the tree starts empty and the baseline is created lazily (first no-parent
/// `create_experiment`).
pub fn create_project(
    store: &Store,
    name: &str,
    path: &str,
    options: CreateProjectOptions,
) -> Result<LocalProject> {
    let CreateProjectOptions {
        create_folder,
        initialize_git,
        clone_url,
        run_command,
        paper_id,
    } = options;
    let slug = unique_project_slug(store, &slugify(name))?;
    let repo_path = prepare_path(path, create_folder, initialize_git, clone_url.as_deref())?;
    if store
        .list_local_projects()?
        .iter()
        .any(|project| Path::new(&project.repo_path) == repo_path)
    {
        return Err(crate::error::anyhow!(
            "{} is already registered as an OpenResearch project",
            repo_path.display()
        ));
    }
    let baseline_branch = git::require_current_branch(&repo_path)?;

    let now = now_ms();
    let project = LocalProject {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.to_string(),
        slug,
        github_owner: String::new(),
        github_repo: String::new(),
        github_sync_enabled: false,
        baseline_branch,
        repo_path: repo_path.to_string_lossy().to_string(),
        run_command: run_command.filter(|c| !c.trim().is_empty()),
        paper_id: paper_id.filter(|p| !p.trim().is_empty()),
        created_at: now,
        updated_at: now,
    };
    store.create_local_project(&project)?;
    Ok(project)
}

#[derive(Default)]
pub struct CreateProjectOptions {
    pub create_folder: bool,
    pub initialize_git: bool,
    pub clone_url: Option<String>,
    pub run_command: Option<String>,
    pub paper_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn root() -> PathBuf {
        std::env::temp_dir().join(format!("orx-local-project-{}", uuid::Uuid::new_v4()))
    }

    fn run_git(path: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(path)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {}", args.join(" "));
    }

    fn initialized(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        run_git(path, &["init", "-b", "main"]);
        std::fs::write(path.join("README.md"), "# test\n").unwrap();
        run_git(path, &["add", "-A"]);
        run_git(
            path,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "initial",
            ],
        );
    }

    #[test]
    fn creates_local_only_project_without_a_remote() {
        let root = root();
        let store = Store::open_at(root.join("data")).unwrap();
        let project_path = root.join("project");
        let project = create_project(
            &store,
            "Local project",
            project_path.to_str().unwrap(),
            CreateProjectOptions {
                create_folder: true,
                initialize_git: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(project.github_owner.is_empty());
        assert!(project.github_repo.is_empty());
        assert_eq!(project.baseline_branch, "main");
        assert_eq!(
            Path::new(&project.repo_path),
            std::fs::canonicalize(&project_path).unwrap()
        );
        assert!(git::remotes(&project_path).unwrap().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn accepts_dirty_but_rejects_detached_repositories() {
        let root = root();
        let dirty = root.join("dirty");
        initialized(&dirty);
        std::fs::write(dirty.join("README.md"), "changed\n").unwrap();
        let store = Store::open_at(root.join("data")).unwrap();
        let project = create_project(
            &store,
            "Dirty",
            dirty.to_str().unwrap(),
            CreateProjectOptions::default(),
        )
        .unwrap();
        assert_eq!(
            Path::new(&project.repo_path),
            std::fs::canonicalize(&dirty).unwrap()
        );
        assert!(!git::is_clean(&dirty).unwrap());

        let detached = root.join("detached");
        initialized(&detached);
        run_git(&detached, &["checkout", "--detach"]);
        let error = create_project(
            &store,
            "Detached",
            detached.to_str().unwrap(),
            CreateProjectOptions::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("detached HEAD"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn paper_clone_keeps_source_as_upstream() {
        let root = root();
        let source = root.join("source");
        initialized(&source);
        let store = Store::open_at(root.join("data")).unwrap();
        let destination = root.join("paper");
        let project = create_project(
            &store,
            "Paper",
            destination.to_str().unwrap(),
            CreateProjectOptions {
                create_folder: true,
                clone_url: Some(source.to_string_lossy().into_owned()),
                paper_id: Some("2401.12345".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        let remotes = git::remotes(Path::new(&project.repo_path)).unwrap();
        assert_eq!(remotes[0].0, "upstream");
        assert!(!remotes.iter().any(|(name, _)| name == "origin"));
        assert!(project.github_owner.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ordinary_remote_is_not_project_metadata() {
        let root = root();
        let project_path = root.join("project");
        initialized(&project_path);
        run_git(
            &project_path,
            &[
                "remote",
                "add",
                "origin",
                "git@github.com:example/research.git",
            ],
        );
        let store = Store::open_at(root.join("data")).unwrap();
        let project = create_project(
            &store,
            "Existing",
            project_path.to_str().unwrap(),
            CreateProjectOptions::default(),
        )
        .unwrap();
        assert!(project.github_owner.is_empty());
        assert!(project.github_repo.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_publication_remote_is_not_project_metadata() {
        let root = root();
        let project_path = root.join("project");
        initialized(&project_path);
        run_git(
            &project_path,
            &[
                "remote",
                "add",
                "github",
                "git@github.com:example/research.git",
            ],
        );
        let store = Store::open_at(root.join("data")).unwrap();
        let project = create_project(
            &store,
            "Existing",
            project_path.to_str().unwrap(),
            CreateProjectOptions::default(),
        )
        .unwrap();
        assert!(project.github_owner.is_empty());
        assert!(project.github_repo.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn nested_folder_resolves_to_repository_root() {
        let root = root();
        let project_path = root.join("project");
        initialized(&project_path);
        let nested = project_path.join("src/nested");
        std::fs::create_dir_all(&nested).unwrap();
        let store = Store::open_at(root.join("data")).unwrap();
        let project = create_project(
            &store,
            "Nested",
            nested.to_str().unwrap(),
            CreateProjectOptions::default(),
        )
        .unwrap();
        assert_eq!(
            Path::new(&project.repo_path),
            std::fs::canonicalize(project_path).unwrap()
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
