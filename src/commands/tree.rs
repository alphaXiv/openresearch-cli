//! The `tree` command.
//!
//! Lists files in an experiment's committed branch under an optional path.
//! Reads Forgejo directly — no open dev node required.

use crate::client::ls_workdir;
use crate::error::{require_credentials, Result};

pub async fn run(args: crate::TreeArgs) -> Result<()> {
    let creds = require_credentials().await;
    let result = ls_workdir(&creds, &args.exp_id, args.path.as_deref()).await?;
    if result.files.is_empty() {
        eprintln!("No files.");
        return Ok(());
    }
    for file in &result.files {
        println!("{}", file.path);
    }
    Ok(())
}
