//! Local experiment creation — slug, branch, row. Shared by the CLI
//! (`orx create-experiment`) and the `orx up` HTTP API.

use std::collections::HashSet;
use std::path::Path;

use crate::error::{anyhow, Result};
use crate::store::{now_ms, Store};

use super::git;
use super::model::{LocalExperiment, LocalProject};
use super::slugify;

/// First free slug within the project: `base`, `base-2`, `base-3`, …
fn unique_slug(store: &Store, project_id: &str, base: &str) -> Result<String> {
    let taken: HashSet<String> = store
        .list_experiments_by_project(project_id)?
        .into_iter()
        .map(|e| e.slug)
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

/// The project's root experiment (parent NULL) — oldest first when several
/// exist. `None` on a fresh project: the tree starts empty and the first
/// no-parent create becomes the baseline. CLI/API creates without a parent
/// attach here once a root exists (pass `--baseline` to add another root).
pub fn project_root(store: &Store, project_id: &str) -> Result<Option<LocalExperiment>> {
    // list is ordered created_at ASC, so `find` picks the oldest root.
    Ok(store
        .list_experiments_by_project(project_id)?
        .into_iter()
        .find(|e| e.parent_experiment_id.is_none()))
}

/// Create a local experiment. With a parent: branch `orx/<slug>` off the
/// parent's tip and push it to origin. Without: a baseline/root row that rides
/// the project's baseline branch (no new branch).
pub fn create_experiment(
    store: &Store,
    project: &LocalProject,
    parent: Option<&LocalExperiment>,
    slug: Option<&str>,
    title: Option<String>,
    description: Option<String>,
    run_command: Option<String>,
) -> Result<LocalExperiment> {
    if let Some(p) = parent {
        if p.project_id != project.id {
            return Err(anyhow!(
                "Parent experiment {} belongs to a different project.",
                p.id
            ));
        }
    }
    let base = match slug {
        Some(s) => slugify(s),
        None => slugify(title.as_deref().unwrap_or("experiment")),
    };
    let slug = unique_slug(store, &project.id, &base)?;

    // Git only on the parented path: a baseline row needs no branch — it rides
    // the project's baseline branch, which was validated at clone time.
    let branch_name = match parent {
        Some(p) => {
            let repo = git::ensure_clone(
                &project.github_owner,
                &project.github_repo,
                &project.baseline_branch,
            )?;
            let branch = format!("orx/{slug}");
            git::create_experiment_branch(Path::new(&repo), &p.branch_name, &branch)?;
            branch
        }
        None => project.baseline_branch.clone(),
    };

    // Inherit: explicit > parent's command > project default > "".
    let run_command = run_command
        .filter(|c| !c.trim().is_empty())
        .or_else(|| {
            parent
                .map(|p| p.run_command.clone())
                .filter(|c| !c.trim().is_empty())
        })
        .or_else(|| project.run_command.clone())
        .unwrap_or_default();

    let now = now_ms();
    let experiment = LocalExperiment {
        id: uuid::Uuid::new_v4().to_string(),
        project_id: project.id.clone(),
        parent_experiment_id: parent.map(|p| p.id.clone()),
        slug,
        branch_name,
        title,
        description,
        run_command,
        agent_status: "idle".to_string(),
        created_at: now,
        updated_at: now,
    };
    store.create_local_experiment(&experiment)?;
    Ok(experiment)
}
