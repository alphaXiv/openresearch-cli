//! Local project creation — clone the repo and insert the project row. Used by
//! the `orx up` HTTP API (`POST /api/projects`).
//!
//! The project starts with an empty experiment tree. The first experiment
//! created without a parent (via `orx create-experiment` or the HTTP API)
//! becomes the baseline root — the control every variant is measured against.

use std::collections::HashSet;

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

/// Clone the repo and create the project row. No experiments are created —
/// the tree starts empty and the baseline is created lazily (first no-parent
/// `create_experiment`).
pub fn create_project(
    store: &Store,
    name: &str,
    github_owner: &str,
    github_repo: &str,
    baseline_branch: Option<String>,
    run_command: Option<String>,
) -> Result<LocalProject> {
    let baseline_branch = baseline_branch
        .filter(|b| !b.trim().is_empty())
        .unwrap_or_else(|| "main".to_string());
    let slug = unique_project_slug(store, &slugify(name))?;
    let repo_path = git::ensure_clone(github_owner, github_repo, &baseline_branch)?;

    let now = now_ms();
    let project = LocalProject {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.to_string(),
        slug,
        github_owner: github_owner.to_string(),
        github_repo: github_repo.to_string(),
        baseline_branch,
        repo_path: repo_path.to_string_lossy().to_string(),
        run_command: run_command.filter(|c| !c.trim().is_empty()),
        created_at: now,
        updated_at: now,
    };
    store.create_local_project(&project)?;
    Ok(project)
}
