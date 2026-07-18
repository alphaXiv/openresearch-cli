//! Project reports: upload a report folder (report.md + images) to a project,
//! or list a project's existing reports.
//!
//! A report on disk is just a folder — typically a `report.md` plus an
//! `images/` subfolder, exactly the shape the autoresearch agent writes. Upload
//! creates the report row, then PUTs each file directly to storage using the
//! presigned URLs the API returns (bytes never transit the API).
//!
//! Reports are a cloud-only feature: a local project has no report registry, so
//! its plane returns files-dir guidance instead of a registry op. The dispatch,
//! upload/list/show/download logic, and the guidance all live in the plane impls
//! (`ServerPlane` / `LocalPlane`); this command just resolves and forwards.

use crate::error::Result;
use crate::plane::resolve_project;

pub async fn run(args: crate::ReportArgs) -> Result<()> {
    let project_id = match &args.command {
        crate::ReportCommand::Upload { project_id, .. }
        | crate::ReportCommand::List { project_id }
        | crate::ReportCommand::Show { project_id, .. }
        | crate::ReportCommand::Download { project_id, .. } => project_id,
    };
    let store = crate::store::Store::open()?;
    let plane = resolve_project(store, project_id)?;
    plane.report(args.command).await
}
