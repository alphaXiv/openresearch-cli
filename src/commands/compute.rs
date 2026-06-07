//! The `compute` command. Lists the GPU offer catalog as a price-sorted table.
//!
//! The catalog spans every provider, but experiment runs launched on a *new*
//! instance (`orx exp run --gpu ...`) are RunPod-only — the server resolves the
//! cheapest matching RunPod offer for the chosen (gpu, count).

use crate::client::list_catalog;
use crate::error::{require_credentials, Result};
use crate::output::print_table;

/// Lists the GPU compute catalog, optionally filtered by gpu id and/or count.
pub async fn run(args: crate::ComputeArgs) -> Result<()> {
    let creds = require_credentials().await;
    let offers = list_catalog(&creds).await?.offers;

    // The API already returns offers sorted by price ascending; keep that order.
    let filtered: Vec<_> = offers
        .into_iter()
        .filter(|o| {
            args.gpu
                .as_ref()
                .is_none_or(|g| o.gpu.eq_ignore_ascii_case(g))
        })
        .filter(|o| args.count.is_none_or(|c| o.gpu_count == c))
        .collect();

    if filtered.is_empty() {
        println!("No matching compute offers.");
        return Ok(());
    }

    let rows: Vec<Vec<String>> = filtered
        .iter()
        .map(|o| {
            vec![
                o.gpu.clone(),
                o.gpu_count.to_string(),
                format!("${:.2}", o.price_per_hour),
                format!("{:.0}", o.vcpus),
                format!("{:.0}", o.ram_gb),
                o.region.clone().unwrap_or_else(|| "—".to_string()),
                o.provider.clone(),
            ]
        })
        .collect();

    print_table(
        &[
            "GPU", "COUNT", "$/HR", "VCPUS", "RAM(GB)", "REGION", "PROVIDER",
        ],
        &rows,
    );

    Ok(())
}
