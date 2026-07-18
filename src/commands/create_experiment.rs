//!
//! Creates an experiment node. Shapes, picked by flags:
//!   --parent <id>   -> child experiment branched off that parent
//!   --baseline      -> a new baseline (root), even when roots already exist —
//!                      projects may hold multiple baselines
//!   (no flags)      -> the oldest project root when one exists (local
//!                      projects), or the baseline (root) when the tree is
//!                      empty — projects start with no experiments, so the
//!                      first create is the baseline: the control its
//!                      variants are measured against
//! A title is always required.
//!
//! Note: the repo a project works on is chosen when the PROJECT is created
//! (`orx create-project` or the web), not here — so there is no `--repo` flag.
//! The baseline is materialized on whatever repo the project is already bound to.

use crate::error::Result;
use crate::plane::{resolve_project, CreateExperimentSpec};
use crate::store::Store;

const USAGE: &str = "Usage: orx create-experiment <projectId> --title \"<title>\" [--parent <experimentId>] [--description \"<text>\"] [--run-command \"<cmd>\"]";

pub async fn run(mut args: crate::CreateExperimentArgs) -> Result<()> {
    let title = match args.title.take() {
        Some(t) => t,
        None => {
            eprintln!("{}", USAGE);
            std::process::exit(1);
        }
    };

    // Local project (orx up): create the row + branch locally, no api — the
    // plane resolver decides which side owns the id.
    let store = Store::open()?;
    let plane = resolve_project(store, &args.project_id)?;
    let is_local = plane.is_local();
    plane
        .create_experiment(CreateExperimentSpec {
            title,
            parent: args.parent,
            baseline: args.baseline,
            description: args.description,
            run_command: args.run_command,
        })
        .await?;
    // Key event, fired only on success. Coarse props only — no ids/names.
    crate::telemetry::capture_experiment_started("create", is_local, None);
    Ok(())
}
