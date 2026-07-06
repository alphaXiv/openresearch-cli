//! The `env` command. Lists the NAMES of the environment variables a run in a
//! project will see (merged org + project vars plus your own per-user vars),
//! annotated by where each is set. Values are never returned by the API — this
//! is purely for discovering which secrets exist (e.g. is `WANDB_API_KEY` set?).

use crate::client::list_env_var_names;
use crate::error::{require_credentials, Result};

/// Lists a project's effective env var names, grouped by source.
pub async fn run(args: crate::EnvArgs) -> Result<()> {
    let store = crate::store::Store::open()?;
    if store.get_local_project(&args.project_id)?.is_some() {
        return Err(crate::local::unsupported("env"));
    }
    let creds = require_credentials().await;
    let mut env_vars = list_env_var_names(&creds, &args.project_id).await?.env_vars;

    if env_vars.is_empty() {
        println!("No environment variables set for this project.");
        return Ok(());
    }

    // Stable, readable ordering: by source (org, then project, then user), then key.
    let rank = |s: &str| match s {
        "org" => 0,
        "project" => 1,
        _ => 2,
    };
    env_vars.sort_by(|a, b| {
        rank(&a.source)
            .cmp(&rank(&b.source))
            .then(a.key.cmp(&b.key))
    });

    let width = env_vars.iter().map(|v| v.key.len()).max().unwrap_or(0);
    for v in &env_vars {
        println!("{:<width$}  ({})", v.key, v.source, width = width);
    }

    Ok(())
}
