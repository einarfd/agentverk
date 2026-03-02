//! agv — create and manage QEMU VMs for AI coding agents.

// These are expected during scaffolding — stubs will gain docs and async
// bodies as features are implemented.
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::unused_async)]

pub mod cli;
pub mod config;
pub mod dirs;
pub mod doctor;
pub mod error;
pub mod image;
pub mod images;
pub mod init;
pub mod specs;
pub mod ssh;
pub mod template;
pub mod vm;

use cli::{CacheCommand, Cli, Command, ConfigCommand, TemplateCommand, TemplateRmArgs};
use specs::SpecSource;
use images::ImageType;

fn config_step_label(step: &config::ProvisionStep) -> String {
    if let Some(ref source) = step.source {
        return source.clone();
    }
    if let Some(ref path) = step.script {
        return path.clone();
    }
    if let Some(ref script) = step.run {
        let first_line = script.lines().next().unwrap_or(script).trim();
        return if first_line.len() > 40 {
            format!("{}...", &first_line[..40])
        } else {
            first_line.to_string()
        };
    }
    "unknown".to_string()
}

/// Run the CLI, dispatching to the appropriate subcommand handler.
#[allow(clippy::too_many_lines)]
pub async fn run(cli: Cli) -> anyhow::Result<()> {
    dirs::ensure_dirs().await?;

    let verbose = cli.verbose;
    let quiet = cli.quiet;

    match cli.command {
        Command::Create(args) => {
            let start = args.start;
            let name = args.name.clone();
            if let Some(ref template_name) = args.from.clone() {
                tracing::info!(name = %name, template = %template_name, "creating VM from template");
                vm::create_from_template(
                    template_name,
                    &name,
                    args.memory.as_deref(),
                    args.cpus,
                    args.disk.as_deref(),
                    start,
                    verbose,
                    quiet,
                )
                .await
            } else {
                let config = config::build_from_cli(&args)?;
                tracing::info!(name = %name, "creating VM");
                vm::create(&name, &config, start, verbose, quiet).await
            }
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
            tracing::info!(name = %args.name, force = args.force, "destroying VM");
            vm::destroy(&args.name, args.force).await?;
            println!("  ✓ VM '{}' destroyed", args.name);
            Ok(())
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
        Command::Cache(args) => match args.command {
            CacheCommand::Ls => {
                let entries = image::list_cache().await?;
                if entries.is_empty() {
                    eprintln!("No cached images.");
                    return Ok(());
                }
                let col_width = entries.iter().map(|e| e.filename.len()).max().unwrap_or(0);
                for e in &entries {
                    let status = if e.in_use { "in use" } else { "unused" };
                    println!(
                        "  {:<col_width$}  {:>10}  {}",
                        e.filename,
                        indicatif::HumanBytes(e.size).to_string(),
                        status,
                    );
                }
                Ok(())
            }
            CacheCommand::Clean => {
                let deleted = image::clean_cache().await?;
                if deleted.is_empty() {
                    println!("  Nothing to clean — all cached images are in use.");
                    return Ok(());
                }
                let total: u64 = deleted.iter().map(|(_, size)| size).sum();
                for (name, size) in &deleted {
                    println!(
                        "  Deleted {}  ({})",
                        name,
                        indicatif::HumanBytes(*size)
                    );
                }
                println!("  Freed {}", indicatif::HumanBytes(total));
                Ok(())
            }
        },
        Command::Config(args) => match args.command {
            ConfigCommand::Show(s) => {
                const W: usize = 10;
                let inst = vm::instance::Instance::open(&s.name).await?;
                let cfg = config::load_resolved(&inst.config_path())?;

                println!("Hardware");
                println!("  {:<W$}  {}", "memory", cfg.memory);
                println!("  {:<W$}  {}", "cpus", cfg.cpus);
                println!("  {:<W$}  {}", "disk", cfg.disk);
                println!("  {:<W$}  {}", "user", cfg.user);

                println!();
                println!("Image");
                if let Some(ref tname) = cfg.template_name {
                    println!("  from template: {tname}");
                } else {
                    println!("  {}", cfg.base_url);
                    if !cfg.skip_checksum {
                        let short = &cfg.base_checksum[..cfg.base_checksum.len().min(20)];
                        println!("  checksum: {short}...");
                    }
                }

                if !cfg.files.is_empty() {
                    println!();
                    println!("Files  ({} entries)", cfg.files.len());
                    for f in &cfg.files {
                        println!("  {} → {}", f.source, f.dest);
                    }
                }

                println!();
                if cfg.setup.is_empty() {
                    println!("Setup        none");
                } else {
                    println!("Setup  ({} steps)", cfg.setup.len());
                    for (i, step) in cfg.setup.iter().enumerate() {
                        let label = config_step_label(step);
                        println!("  {}. {label}", i + 1);
                    }
                }

                println!();
                if cfg.provision.is_empty() {
                    println!("Provision    none");
                } else {
                    println!("Provision  ({} steps)", cfg.provision.len());
                    for (i, step) in cfg.provision.iter().enumerate() {
                        let label = config_step_label(step);
                        println!("  {}. {label}", i + 1);
                    }
                }

                Ok(())
            }
            ConfigCommand::Set(s) => {
                let inst_config = {
                    let inst = vm::instance::Instance::open(&s.name).await?;
                    config::load_resolved(&inst.config_path())?
                };
                let old_memory = inst_config.memory.clone();
                let old_cpus = inst_config.cpus;
                let old_disk = inst_config.disk.clone();

                vm::config_set(
                    &s.name,
                    s.memory.as_deref(),
                    s.cpus,
                    s.disk.as_deref(),
                )
                .await?;

                // Report what changed.
                if let Some(ref m) = s.memory {
                    println!("  memory:  {old_memory} → {m}");
                }
                if let Some(n) = s.cpus {
                    println!("  cpus:    {old_cpus} → {n}");
                }
                if let Some(ref d) = s.disk {
                    println!("  disk:    {old_disk} → {d}");
                    println!(
                        "  Note: guest filesystem not resized — run growpart/resize2fs \
                         inside the VM to use the extra space."
                    );
                }
                println!("  ✓ VM '{}' updated", s.name);
                Ok(())
            }
        },
        Command::Specs => {
            let all = specs::list_all()?;
            if all.is_empty() {
                eprintln!("No specs found.");
                return Ok(());
            }
            for s in &all {
                print!(
                    "  {:<8}  {:>4} RAM  {:>2} vCPU  {:>5} disk",
                    s.name, s.spec.memory, s.spec.cpus, s.spec.disk
                );
                if let SpecSource::User(path) = &s.source {
                    print!("  ({})", path.display());
                }
                println!();
            }
            Ok(())
        }
        Command::Template(args) => match args.command {
            TemplateCommand::Create(targs) => {
                tracing::info!(
                    vm = %targs.vm,
                    template = %targs.name,
                    "creating template"
                );
                vm::create_template(&targs.vm, &targs.name, targs.stop, verbose, quiet).await
            }
            TemplateCommand::Ls => {
                let templates = vm::list_templates().await?;
                if templates.is_empty() {
                    eprintln!("No templates found. Create one with: agv template create <vm> <name>");
                    return Ok(());
                }
                let col_width = templates.iter().map(|t| t.name.len()).max().unwrap_or(0);
                for t in &templates {
                    let deps = if t.dependents.is_empty() {
                        "unused".to_string()
                    } else {
                        format!("used by: {}", t.dependents.join(", "))
                    };
                    println!(
                        "  {:<col_width$}  {}  {} vCPUs  {} disk  (from {})  {}",
                        t.name, t.memory, t.cpus, t.disk, t.source_vm, deps
                    );
                }
                Ok(())
            }
            TemplateCommand::Rm(TemplateRmArgs { name }) => {
                tracing::info!(template = %name, "removing template");
                vm::remove_template(&name).await?;
                println!("  ✓ Template '{name}' deleted");
                Ok(())
            }
        },
        Command::Doctor => doctor::run(),
        Command::Init(args) => init::run(args.template.as_deref(), args.force),
    }
}
