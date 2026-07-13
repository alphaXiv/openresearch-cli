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
/// `project` is never issued — it's the files dir's reserved top-level
/// namespace for project-wide reports, and experiment folders there are
/// keyed by slug.
fn unique_slug(store: &Store, project_id: &str, base: &str) -> Result<String> {
    let mut taken: HashSet<String> = store
        .list_experiments_by_project(project_id)?
        .into_iter()
        .map(|e| e.slug)
        .collect();
    taken.insert(super::files::PROJECT_NAMESPACE.to_string());
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

/// Warning for pre-orx/<slug>-baseline rows: roots created before baselines
/// got their own branch ride the project's base branch, so that branch is an
/// immutable experiment node. `None` for experiments created since.
pub fn legacy_root_warning(project: &LocalProject, experiment: &LocalExperiment) -> Option<String> {
    (experiment.parent_experiment_id.is_none() && experiment.branch_name == project.baseline_branch)
        .then(|| {
            format!(
                "warning: root experiment {} rides the project's base branch '{}' \
                 (created before baselines got their own orx/* branch). Treat '{}' \
                 as frozen — publish READMEs/notebooks elsewhere, or recreate the \
                 tree with `orx create-experiment --baseline`.",
                experiment.id, project.baseline_branch, project.baseline_branch
            )
        })
}

/// Create a local experiment. Every node gets its own `orx/<slug>` branch,
/// pushed to origin: a child forks off its parent's tip, a baseline/root off
/// the project's base branch. The base branch itself is never an experiment
/// node — it stays mutable (README, notebooks, publication surface) while
/// `orx/*` branches hold the frozen experiment code. Matches the server path,
/// which also branches baselines to `orx/<slug>`.
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

    let repo = git::ensure_clone(
        &project.github_owner,
        &project.github_repo,
        &project.baseline_branch,
    )?;
    let fork_point = parent
        .map(|p| p.branch_name.as_str())
        .unwrap_or(&project.baseline_branch);
    let branch_name = format!("orx/{slug}");
    git::create_experiment_branch(Path::new(&repo), fork_point, &branch_name)?;

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

#[cfg(test)]
mod tests {
    use super::*;

    fn project(base: &str) -> LocalProject {
        LocalProject {
            id: "p1".into(),
            name: "Demo".into(),
            slug: "demo".into(),
            github_owner: "o".into(),
            github_repo: "r".into(),
            baseline_branch: base.into(),
            repo_path: "/tmp/r".into(),
            run_command: None,
            paper_id: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn experiment(parent: Option<&str>, branch: &str) -> LocalExperiment {
        LocalExperiment {
            id: "e1".into(),
            project_id: "p1".into(),
            parent_experiment_id: parent.map(String::from),
            slug: "baseline".into(),
            branch_name: branch.into(),
            title: None,
            description: None,
            run_command: String::new(),
            agent_status: "idle".into(),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn warns_only_for_legacy_roots_on_the_base_branch() {
        let p = project("main");
        // Legacy root riding main: warn.
        let w = legacy_root_warning(&p, &experiment(None, "main")).unwrap();
        assert!(w.contains("rides the project's base branch 'main'"));
        // Current-scheme root on its own branch: silent.
        assert!(legacy_root_warning(&p, &experiment(None, "orx/baseline")).is_none());
        // Child, even on the base branch name (not a root): silent.
        assert!(legacy_root_warning(&p, &experiment(Some("root"), "main")).is_none());
    }
}
