//! The `experiments` command. Lists a project's experiments as an indented
//! tree (by parentExperimentId).

use std::collections::{HashMap, HashSet};

use crate::client::{list_experiments, Experiment};
use crate::error::{require_credentials, Result};

/// Lists a project's experiments as an indented tree (by parentExperimentId).
pub async fn run(args: crate::ExperimentsArgs) -> Result<()> {
    let creds = require_credentials().await;
    let experiments = list_experiments(&creds, &args.project_id)
        .await?
        .experiments;

    if experiments.is_empty() {
        println!("No experiments in this project.");
        return Ok(());
    }

    // Group children by parent so we can walk the tree from the roots down.
    let mut children_of: HashMap<Option<String>, Vec<usize>> = HashMap::new();
    for (idx, exp) in experiments.iter().enumerate() {
        children_of
            .entry(exp.parent_experiment_id.clone())
            .or_default()
            .push(idx);
    }

    // A "root" is anything whose parent isn't present in this project's set
    // (covers both true roots and children whose parent was filtered out).
    let ids: HashSet<&str> = experiments.iter().map(|e| e.id.as_str()).collect();
    let roots: Vec<usize> = experiments
        .iter()
        .enumerate()
        .filter(|(_, e)| match &e.parent_experiment_id {
            None => true,
            Some(pid) => !ids.contains(pid.as_str()),
        })
        .map(|(idx, _)| idx)
        .collect();

    for root in roots {
        print_node(&experiments, &children_of, root, 0);
    }

    Ok(())
}

fn print_node(
    experiments: &[Experiment],
    children_of: &HashMap<Option<String>, Vec<usize>>,
    idx: usize,
    depth: usize,
) {
    let exp = &experiments[idx];
    let indent = "  ".repeat(depth);
    println!("{indent}\u{25b8} {}  ({})", exp.title, exp.status);
    if let Some(children) = children_of.get(&Some(exp.id.clone())) {
        for &child in children {
            print_node(experiments, children_of, child, depth + 1);
        }
    }
}
