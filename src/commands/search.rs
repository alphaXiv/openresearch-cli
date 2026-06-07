//! The `search` command.
//!
//! Greps an experiment's committed branch for a case-insensitive substring.
//! Reads Forgejo directly, so unlike `orx grep` it needs no open dev node.

use crate::error::Result;

pub async fn run(args: crate::SearchArgs) -> Result<()> {
    let creds = crate::error::require_credentials().await;
    let output = crate::client::search_workdir(&creds, &args.exp_id, &args.query).await?;
    println!("{}", output.output);
    Ok(())
}
