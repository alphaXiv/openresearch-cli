//! Single crate error type.
//!
//! Commands propagate failures with the `?` operator. We use `anyhow::Error`
//! as the crate-wide error so any error (HTTP, IO, serde, custom messages)
//! flows through one channel. `main` prints the error's `Display` to stderr
//! and exits 1, matching the TS entry point's
//! `console.error(err.message); process.exit(1)`.

// Convenience re-exports forming the crate's error vocabulary. Not all are used
// today, but command modules build against this surface.
#[allow(unused_imports)]
pub use anyhow::{anyhow, bail, Context, Error, Result};

use crate::config::{load_credentials, Credentials};

/// Loads stored credentials or exits the process with code 1 and the message
/// `Not logged in. Run `orx login` first.` (stderr), exactly like the TS
/// `requireCredentials`.
///
/// This intentionally returns `Credentials` (not `Result`) and `exit`s on the
/// missing-credentials path so command authors can write:
///
/// ```ignore
/// let creds = require_credentials().await;
/// ```
///
/// IO errors while *reading* an existing file are treated as "not logged in",
/// matching the TS behavior where `loadCredentials` swallows errors and returns
/// `null`.
pub async fn require_credentials() -> Credentials {
    match load_credentials().await {
        Ok(Some(creds)) => creds,
        _ => {
            eprintln!("Not logged in. Run `orx login` first.");
            std::process::exit(1);
        }
    }
}
