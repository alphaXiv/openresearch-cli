//! The `project` command group: operate on a single project by id.
//!
//!   orx project edit <projectId> [--name …] [--description … | --description-stdin] [--public | --private]
//!
//! Sibling to `orx projects` (which lists): the plural lists, the singular edits
//! one — mirroring `orx experiments` (list) vs `orx exp` (operate). Project ids
//! come from `orx projects`.

use crate::error::Result;
use crate::plane::{resolve_project, ProjectEdit};
use crate::ProjectCommand;

pub async fn run(args: crate::ProjectArgs) -> Result<()> {
    let project_id = match &args.command {
        ProjectCommand::View { project_id } | ProjectCommand::Edit { project_id, .. } => project_id,
    };
    let store = crate::store::Store::open()?;
    let plane = resolve_project(store, project_id)?;
    match args.command {
        ProjectCommand::View { .. } => plane.view_project().await,
        ProjectCommand::Edit {
            name,
            description,
            description_stdin,
            public,
            private,
            run_command,
            ..
        } => {
            plane
                .edit_project(ProjectEdit {
                    name,
                    description,
                    description_stdin,
                    public,
                    private,
                    run_command,
                })
                .await
        }
    }
}
