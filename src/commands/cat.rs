//!
//! Prints a file's content from an experiment's committed branch. Reads the
//! workdir directly — no open dev node required.

use std::io::Write;

use crate::error::{require_credentials, Result};

pub async fn run(args: crate::CatArgs) -> Result<()> {
    let creds = require_credentials().await;
    let read = crate::client::read_workdir(&creds, &args.exp_id, &args.path).await?;
    let content = read.content;

    let mut stdout = std::io::stdout();
    stdout.write_all(content.as_bytes())?;
    if !content.is_empty() && !content.ends_with('\n') {
        stdout.write_all(b"\n")?;
    }
    stdout.flush()?;
    Ok(())
}
