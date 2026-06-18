//! The `compute` command. Lists the GPU offer catalog as a price-sorted table,
//! or the CPU-only catalog with `--cpu`.
//!
//! The GPU catalog endpoint (`GET /compute/catalog`) spans every configured
//! provider and region; this command lists all of them, sorted by price
//! ascending (the order the API returns), and can be narrowed with `--gpu`,
//! `--count`, and `--provider`. Any provider shown here is launchable as a new
//! instance via `orx exp run --gpu ... --provider ...` or `orx instance create`.
//! CPU offers (`--cpu`) are RunPod-only.

use crate::client::{list_catalog, list_cpu_catalog, Disk};
use crate::error::{require_credentials, Result};
use crate::output::print_table;

/// Lists the compute catalog. With `--cpu`, lists CPU-only offers; otherwise the
/// GPU catalog, optionally filtered by gpu id and/or count.
pub async fn run(args: crate::ComputeArgs) -> Result<()> {
    let creds = require_credentials().await;

    if args.cpu {
        return run_cpu(&creds).await;
    }

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
        .filter(|o| {
            args.provider
                .as_ref()
                .is_none_or(|p| o.provider.eq_ignore_ascii_case(p))
        })
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
                fmt_disk(&o.disk),
                format!("{:.0}", o.vcpus),
                format!("{:.0}", o.ram_gb),
                o.region.clone().unwrap_or_else(|| "—".to_string()),
                o.provider.clone(),
            ]
        })
        .collect();

    print_table(
        &[
            "GPU", "COUNT", "$/HR", "DISK", "VCPUS", "RAM(GB)", "REGION", "PROVIDER",
        ],
        &rows,
    );

    Ok(())
}

/// Formats an offer's disk pricing for the `DISK` column: sizable offers show a
/// per-GB/hour rate, fixed offers show the bundled capacity. Falls back to `—`
/// if the expected payload is missing for the given `sizable` flag.
fn fmt_disk(disk: &Disk) -> String {
    let value = if disk.sizable {
        disk.per_gb_hour.map(|r| format!("${:.4}/GB·hr", r))
    } else {
        disk.included_gb.map(|gb| format!("{:.0}GB incl", gb))
    };
    value.unwrap_or_else(|| "—".to_string())
}

/// Lists the CPU-only offer catalog as a price-sorted table. Launch one with
/// `orx exp run --cpu <flavor> --vcpus <n>`.
async fn run_cpu(creds: &crate::config::Credentials) -> Result<()> {
    let offers = list_cpu_catalog(creds).await?.offers;

    if offers.is_empty() {
        println!("No CPU compute offers available.");
        return Ok(());
    }

    let rows: Vec<Vec<String>> = offers
        .iter()
        .map(|o| {
            vec![
                o.cpu_flavor.clone(),
                format!("{:.0}", o.vcpus),
                format!("${:.2}", o.price_per_hour),
                fmt_disk(&o.disk),
                format!("{:.0}", o.ram_gb),
                o.region.clone().unwrap_or_else(|| "—".to_string()),
                o.provider.clone(),
            ]
        })
        .collect();

    print_table(
        &[
            "FLAVOR", "VCPUS", "$/HR", "DISK", "RAM(GB)", "REGION", "PROVIDER",
        ],
        &rows,
    );

    Ok(())
}
