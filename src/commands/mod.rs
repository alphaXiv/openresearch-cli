//! Command modules.
//!
//! Convention (every command author MUST follow this):
//!
//!   pub async fn run(args: crate::<Args>) -> crate::error::Result<()>
//!
//! - The arg struct is the clap-derive `Args` type defined in `main.rs` for that
//!   command (e.g. `crate::ProjectsArgs`). It is moved in by value.
//! - The fn loads credentials itself via `crate::error::require_credentials().await`
//!   (which exits 1 with "Not logged in..." when absent) — mirroring the TS, where
//!   each command calls `requireCredentials()`. `login`/`logout` do NOT require
//!   creds; `skill` uses `load_credentials` for best-effort behavior.
//! - Return `Ok(())` on success; propagate errors with `?`. `main` prints the
//!   error and exits 1.
//! - For early-exit "usage" errors that the TS prints to stderr + exit(1),
//!   return `Err(anyhow!(...))` (clap already enforces required positionals, so
//!   most of those usage guards are unnecessary in the Rust port).
//!
//! The six fs verbs share one module (`fs`); `fs::run` takes a
//! `crate::FsInvocation` carrying the verb + positionals.

pub mod artifact;
pub mod artifacts;
pub mod cat;
pub mod chart;
pub mod compute;
pub mod create_experiment;
pub mod dev;
pub mod diff;
pub mod exp;
pub mod experiments;
pub mod fs;
pub mod login;
pub mod logout;
pub mod logs;
pub mod projects;
pub mod query;
pub mod runs;
pub mod search;
pub mod search_logs;
pub mod skill;
pub mod tree;
pub mod wandb;
