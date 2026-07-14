//! The `instance` command group: standalone compute in an organization,
//! independent of any experiment.
//!
//!   orx instance create <orgId> --gpu <id> [--count N] [--disk GB] [--provider P]
//!   orx instance create <orgId> --cpu <cpu5c|cpu5g|cpu5m> [--vcpus 2|8|32]
//!   orx instance list <orgId>
//!   orx instance delete <sandboxId>
//!
//! Create is the CLI equivalent of the dashboard's org "Spin up" panel — it
//! hits `POST /sandboxes`, which provisions an org-level (not project-scoped)
//! box. List/delete double as the cleanup path for a `--backend openresearch`
//! box whose automatic teardown failed.

use crate::client::{
    create_sandbox, delete_sandbox, list_sandboxes, CreateSandboxBody, SandboxTarget,
};
use crate::error::{anyhow, require_credentials, Result};
use crate::{InstanceCommand, InstanceCreateArgs, InstanceDeleteArgs, InstanceListArgs};

pub async fn run(args: crate::InstanceArgs) -> Result<()> {
    let creds = require_credentials().await;
    match args.command {
        InstanceCommand::Create(create_args) => create(&creds, create_args).await,
        InstanceCommand::List(list_args) => list(&creds, list_args).await,
        InstanceCommand::Delete(delete_args) => delete(&creds, delete_args).await,
    }
}

/// `orx instance list <orgId>` — every box in the org, one line each.
async fn list(creds: &crate::config::Credentials, args: InstanceListArgs) -> Result<()> {
    let sandboxes = list_sandboxes(creds, &args.org_id).await?.sandboxes;
    if sandboxes.is_empty() {
        println!("No instances in org {}.", args.org_id);
        return Ok(());
    }
    for s in &sandboxes {
        let shape = match (&s.gpu, s.vcpu_count) {
            (Some(gpu), _) => format!("{} x{}", gpu, s.gpu_count.unwrap_or(1)),
            (None, Some(vcpus)) => format!("{vcpus} vcpus"),
            _ => "byom".to_string(),
        };
        let price = s
            .price_per_hour
            .map(|p| format!("  ${p:.2}/hr"))
            .unwrap_or_default();
        let endpoint = match (&s.ssh_username, &s.ssh_hostname, s.ssh_port) {
            (Some(user), Some(host), Some(port)) => format!("  {user}@{host}:{port}"),
            _ => String::new(),
        };
        println!("{}  {:<12}  {shape}{price}{endpoint}", s.id, s.status);
    }
    Ok(())
}

/// `orx instance delete <sandboxId>` — terminate the box and its provider machine.
async fn delete(creds: &crate::config::Credentials, args: InstanceDeleteArgs) -> Result<()> {
    delete_sandbox(creds, &args.sandbox_id).await?;
    println!("\u{2713} Instance {} deleted.", args.sandbox_id);
    Ok(())
}

/// `orx instance create <orgId> …` — provision a standalone GPU or CPU instance.
async fn create(creds: &crate::config::Credentials, args: InstanceCreateArgs) -> Result<()> {
    // Resolve the target: exactly one of --gpu or --cpu.
    if args.gpu.is_some() && args.cpu.is_some() {
        return Err(anyhow!("Pass exactly one of --gpu or --cpu."));
    }
    if args.provider.is_some() && args.gpu.is_none() {
        return Err(anyhow!(
            "--provider only applies with --gpu (it selects among new GPU offers)."
        ));
    }
    let target = if let Some(gpu) = &args.gpu {
        SandboxTarget::New {
            gpu: gpu.clone(),
            gpu_count: args.count.unwrap_or(1),
            disk_gb: args.disk.unwrap_or(100),
            // Omitted = the server picks the cheapest matching offer across all
            // providers (matching the dashboard's spin-up). The server validates
            // the name and 400s on an unknown provider, so no client-side check.
            provider: args.provider.clone(),
        }
    } else if let Some(cpu_flavor) = &args.cpu {
        SandboxTarget::NewCpu {
            cpu_flavor: cpu_flavor.clone(),
            vcpu_count: args.vcpus.unwrap_or(8),
        }
    } else {
        return Err(anyhow!(
            "Choose compute: --gpu <id> [--count N] [--disk GB] [--provider P], \
             or --cpu <cpu5c|cpu5g|cpu5m> [--vcpus 2|8|32]. \
             See `orx compute` for available GPUs."
        ));
    };

    let sandbox = create_sandbox(
        creds,
        &CreateSandboxBody {
            organization_id: args.org_id.clone(),
            target,
        },
    )
    .await?
    .sandbox;

    println!("\u{2713} Instance requested");
    println!("  id:       {}", sandbox.id);
    println!("  status:   {}", sandbox.status);
    println!("  type:     {}", sandbox.machine_type);
    if let Some(provider) = &sandbox.provider_name {
        println!("  provider: {}", provider);
    }
    match (&sandbox.gpu, sandbox.vcpu_count) {
        (Some(gpu), _) => {
            println!("  gpu:      {} x{}", gpu, sandbox.gpu_count.unwrap_or(1));
        }
        (None, Some(vcpus)) => println!("  vcpus:    {}", vcpus),
        _ => {}
    }
    if let Some(price) = sandbox.price_per_hour {
        println!("  price:    ${:.2}/hr", price);
    }
    // sshHostname is null until the box finishes provisioning, so there's no
    // host to print yet — and there's no `orx` command to poll it.
    println!("\n  The box is provisioning; its SSH host appears once it's online.");

    Ok(())
}
