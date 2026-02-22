//! agv — create and manage QEMU VMs for AI coding agents.

// These are expected during scaffolding — stubs will gain docs and async
// bodies as features are implemented.
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::unused_async)]

pub mod cli;
pub mod config;
pub mod dirs;
pub mod error;
pub mod image;
pub mod images;
pub mod ssh;
pub mod template;
pub mod vm;

use cli::{Cli, Command};
use images::ImageType;

/// Run the CLI, dispatching to the appropriate subcommand handler.
pub async fn run(cli: Cli) -> anyhow::Result<()> {
    dirs::ensure_dirs().await?;

    let verbose = cli.verbose;
    let quiet = cli.quiet;

    match cli.command {
        Command::Create(args) => {
            let start = args.start;
            let name = args.name.clone();
            let config = config::build_from_cli(&args)?;
            tracing::info!(name = %name, "creating VM");
            vm::create(&name, &config, start, verbose, quiet).await
        }
        Command::Start(args) => {
            tracing::info!(name = %args.name, "starting VM");
            vm::start(&args.name, verbose, quiet).await
        }
        Command::Stop(args) => {
            tracing::info!(name = %args.name, force = args.force, "stopping VM");
            vm::stop(&args.name, args.force).await
        }
        Command::Destroy(args) => {
            tracing::info!(name = %args.name, "destroying VM");
            vm::destroy(&args.name).await
        }
        Command::Ssh(args) => {
            let inst = vm::instance::Instance::open(&args.name).await?;
            let cfg = config::load_resolved(&inst.config_path())?;
            ssh::session(&inst, &cfg.user, &args.command).await
        }
        Command::Ls => {
            let instances = vm::list().await?;
            if instances.is_empty() {
                eprintln!("No VMs found. Create one with: agv create <name>");
                return Ok(());
            }
            let col_width = instances.iter().map(|i| i.name.len()).max().unwrap_or(0);
            for inst in &instances {
                let status = inst
                    .reconcile_status()
                    .await
                    .map_or_else(|_| "unknown".to_string(), |s| s.to_string());
                println!("  {:<col_width$}  {status}", inst.name);
            }
            Ok(())
        }
        Command::Images => {
            let all = images::list_all()?;
            if all.is_empty() {
                eprintln!("No images found.");
                return Ok(());
            }
            let (base_images, mixins): (Vec<_>, Vec<_>) = all
                .into_iter()
                .partition(|i| i.image_type == ImageType::Image);

            if !base_images.is_empty() {
                println!("Images");
                for img in &base_images {
                    print!("  {}", img.name);
                    if let images::ImageSource::User(path) = &img.source {
                        print!("  ({})", path.display());
                    }
                    println!();
                }
            }
            if !mixins.is_empty() {
                if !base_images.is_empty() {
                    println!();
                }
                println!("Mixins");
                for img in &mixins {
                    print!("  {}", img.name);
                    if let images::ImageSource::User(path) = &img.source {
                        print!("  ({})", path.display());
                    }
                    println!();
                }
            }
            Ok(())
        }
        Command::Inspect(args) => {
            vm::inspect(&args.name).await
        }
        Command::Snapshot(args) => {
            tracing::info!(name = %args.name, label = ?args.label, "taking snapshot");
            eprintln!("agv snapshot: not yet implemented");
            Ok(())
        }
        Command::Restore(args) => {
            tracing::info!(name = %args.name, label = ?args.label, "restoring snapshot");
            eprintln!("agv restore: not yet implemented");
            Ok(())
        }
    }
}
