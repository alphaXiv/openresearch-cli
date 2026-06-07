//! The file-edit verbs (read/write/str-replace/ls/grep/rm).
//!
//! Each maps to one `dev/fs` op against the experiment's live dev working tree.
//! All require an open dev node (`orx dev open`); the API returns a clear error
//! otherwise.

use tokio::io::AsyncReadExt;

use crate::client::{dev_fs, DevFsOp};
use crate::error::{require_credentials, Result};
use crate::{FsInvocation, FsVerb};

async fn read_stdin() -> Result<String> {
    let mut buf = String::new();
    tokio::io::stdin().read_to_string(&mut buf).await?;
    Ok(buf)
}

pub async fn run(inv: FsInvocation) -> Result<()> {
    let FsInvocation { verb, exp_id, rest } = inv;
    let creds = require_credentials().await;

    let op: DevFsOp = match verb {
        FsVerb::Read => DevFsOp::Read {
            path: rest[0].clone(),
        },
        FsVerb::Write => DevFsOp::Write {
            path: rest[0].clone(),
            content: read_stdin().await?,
        },
        FsVerb::StrReplace => DevFsOp::StrReplace {
            path: rest[0].clone(),
            old_string: rest[1].clone(),
            new_string: rest[2].clone(),
        },
        FsVerb::Ls => DevFsOp::List {
            path: rest.first().cloned(),
        },
        FsVerb::Grep => DevFsOp::Search {
            query: rest[0].clone(),
        },
        FsVerb::Rm => DevFsOp::Delete {
            path: rest[0].clone(),
        },
    };

    let output = dev_fs(&creds, &exp_id, &op).await?.output;
    println!("{}", output);
    Ok(())
}
