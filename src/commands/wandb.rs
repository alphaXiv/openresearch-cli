//! The `wandb` command — list the W&B runs linked to an OpenResearch run.
//!
//! This is the discovery primitive: it tells you which W&B runs exist for a run
//! and their dashboard URLs. For numeric summaries (min/max/last per metric)
//! use `orx chart wandb`; for cached metric history use `orx query`.

use crate::client::list_wandb_runs;
use crate::error::require_credentials;
use crate::error::Result;

pub async fn run(args: crate::WandbArgs) -> Result<()> {
    let creds = require_credentials().await;

    let runs = list_wandb_runs(&creds, &args.run_id).await?.wandb_runs;
    if runs.is_empty() {
        println!("No W&B runs linked to this run.");
        return Ok(());
    }

    for r in &runs {
        // baseUrl is the W&B host; the run lives at <base>/<entity>/<project>/runs/<id>.
        let base = r.base_url.trim_end_matches('/');
        let url = format!(
            "{}/{}/{}/runs/{}",
            base, r.entity, r.project, r.wandb_run_id
        );
        println!("{}/{}  {}", r.entity, r.project, r.wandb_run_id);
        println!("  {}", url);
    }

    eprintln!(
        "\n{} linked W&B run(s). Numeric summaries: `orx chart wandb`; history: `orx query`.",
        runs.len()
    );
    Ok(())
}
