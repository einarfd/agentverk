//! agv — create and manage QEMU VMs for AI agents.

#![expect(
    clippy::missing_errors_doc,
    reason = "application code, not a library: ~70 # Errors blocks would add little for callers"
)]

pub mod cli;
pub mod config;
pub mod dirs;
pub mod doctor;
pub mod error;
pub mod forward;
pub mod forward_daemon;
pub mod image;
pub mod images;
pub mod init;
pub mod interactive;
pub mod specs;
pub mod ssh;
pub mod ssh_config;
pub mod template;
pub mod vm;

use cli::{CacheCommand, Cli, Command, ConfigCommand, TemplateCommand, TemplateRmArgs};
use specs::SpecSource;
use images::ImageType;

/// Build a "VM not running" error message that suggests the right command
/// for the current status (start vs resume).
fn not_running_error(name: &str, status: vm::instance::Status) -> anyhow::Error {
    let action = if status == vm::instance::Status::Suspended {
        format!("Resume it with: agv resume {name}")
    } else {
        format!("Start it with: agv start {name}")
    };
    anyhow::anyhow!("VM '{name}' is not running (status: {status}). {action}")
}

/// Split `agv ssh` trailing args at `--`.
///
/// Everything before `--` is passed to ssh before the destination (ssh options
/// such as `-A`, `-L port:host:port`). Everything after `--` is the remote
/// command, passed after the destination. With no `--`, all args are treated
/// as ssh options and no remote command is run.
fn split_ssh_args(args: &[String]) -> (&[String], &[String]) {
    match args.iter().position(|a| a == "--") {
        Some(i) => (&args[..i], &args[i + 1..]),
        None => (args, &[]),
    }
}

/// Format a byte count as `<n>K`, `<n>M`, `<n.n>G`, or `<n.n>T` to match
/// the size strings used in agv config files (e.g. "8G", "512M", "20G").
fn format_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    const TIB: u64 = 1024 * GIB;
    if bytes >= TIB {
        #[expect(clippy::cast_precision_loss, reason = "display formatting")]
        let v = bytes as f64 / TIB as f64;
        format!("{v:.1}T")
    } else if bytes >= GIB {
        #[expect(clippy::cast_precision_loss, reason = "display formatting")]
        let v = bytes as f64 / GIB as f64;
        format!("{v:.1}G")
    } else if bytes >= MIB {
        format!("{}M", bytes / MIB)
    } else if bytes >= KIB {
        format!("{}K", bytes / KIB)
    } else {
        format!("{bytes}B")
    }
}

async fn forward_command(args: cli::ForwardArgs, quiet: bool) -> anyhow::Result<()> {
    if args.list {
        let active = vm::forwarding::list(&args.name).await?;
        if active.is_empty() {
            if !quiet {
                println!("No active forwards on '{}'.", args.name);
            }
            return Ok(());
        }
        let host_width = active
            .iter()
            .map(|a| a.host.to_string().len())
            .max()
            .unwrap_or(0);
        for a in &active {
            let arrow = if a.host == a.guest { "↔" } else { "→" };
            println!(
                "  host:{host:>w$} {arrow} VM:{guest} ({proto}, {origin})",
                host = a.host,
                w = host_width,
                guest = a.guest,
                proto = a.proto,
                origin = a.origin,
            );
        }
        return Ok(());
    }

    if args.stop {
        if args.ports.is_empty() {
            let removed = vm::forwarding::stop_all(&args.name).await?;
            if !quiet {
                if removed.is_empty() {
                    println!("No active forwards to stop on '{}'.", args.name);
                } else {
                    println!(
                        "  ✓ Stopped {} forward{} on '{}'",
                        removed.len(),
                        if removed.len() == 1 { "" } else { "s" },
                        args.name
                    );
                }
            }
        } else {
            let specs = forward::parse_specs(&args.ports)?;
            let removed = vm::forwarding::stop(&args.name, &specs).await?;
            if !quiet {
                for entry in &removed {
                    println!(
                        "  ✓ Removed host:{host} (was {spec}, {origin})",
                        host = entry.host,
                        spec = entry.spec(),
                        origin = entry.origin,
                    );
                }
            }
        }
        return Ok(());
    }

    // Add path.
    if args.ports.is_empty() {
        anyhow::bail!(
            "no ports specified — pass ports to add, or use --list/--stop (see `agv forward --help`)"
        );
    }
    let specs = forward::parse_specs(&args.ports)?;
    let added = vm::forwarding::add(&args.name, &specs).await?;
    if !quiet {
        for entry in &added {
            let arrow = if entry.host == entry.guest { "↔" } else { "→" };
            println!(
                "  ✓ host:{host} {arrow} VM:{guest} ({proto})",
                host = entry.host,
                guest = entry.guest,
                proto = entry.proto,
            );
        }
    }
    Ok(())
}

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
#[expect(
    clippy::too_many_lines,
    reason = "main subcommand dispatcher; splitting into per-command functions would add boilerplate without improving readability"
)]
pub async fn run(cli: Cli) -> anyhow::Result<()> {
    dirs::ensure_dirs().await?;

    let verbose = cli.verbose;
    let quiet = cli.quiet;

    match cli.command {
        Command::Create(args) => {
            let start = args.start;
            let interactive = args.interactive;
            let name = args.name.clone();
            if interactive && args.from.is_some() {
                anyhow::bail!("--interactive cannot be combined with --from (template clones do not run provisioning)");
            }
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
                vm::create(&name, &config, start, interactive, verbose, quiet).await
            }
        }
        Command::Start(args) => {
            tracing::info!(name = %args.name, retry = args.retry, "starting VM");
            vm::start(&args.name, args.retry, args.interactive, verbose, quiet).await
        }
        Command::Stop(args) => {
            tracing::info!(name = %args.name, force = args.force, "stopping VM");
            vm::stop(&args.name, args.force).await
        }
        Command::Suspend(args) => {
            tracing::info!(name = %args.name, "suspending VM");
            vm::suspend(&args.name).await?;
            println!("  ✓ VM '{}' suspended", args.name);
            Ok(())
        }
        Command::Resume(args) => {
            tracing::info!(name = %args.name, "resuming VM");
            vm::resume(&args.name, verbose, quiet).await
        }
        Command::Destroy(args) => {
            tracing::info!(name = %args.name, force = args.force, "destroying VM");
            vm::destroy(&args.name, args.force).await?;
            println!("  ✓ VM '{}' destroyed", args.name);
            Ok(())
        }
        Command::Rename(args) => {
            tracing::info!(old = %args.old, new = %args.new, "renaming VM");
            vm::rename(&args.old, &args.new).await?;
            println!("  ✓ VM '{}' renamed to '{}'", args.old, args.new);
            println!();
            println!("  Note: the hostname inside the guest is unchanged.");
            println!("  To update it, SSH in after starting the VM and run:");
            println!("    sudo hostnamectl set-hostname {}", args.new);
            Ok(())
        }
        Command::Ssh(args) => {
            let inst = vm::instance::Instance::open(&args.name)?;
            let status = inst.reconcile_status().await?;
            // Allow SSH to a broken VM if QEMU is still running and SSH came
            // up — this lets users debug provisioning failures.
            let broken_but_reachable = status == vm::instance::Status::Broken
                && inst.is_process_alive().await
                && inst.read_provision_state().await.phase != vm::instance::Phase::SshWait;
            if status != vm::instance::Status::Running && !broken_but_reachable {
                return Err(not_running_error(&args.name, status));
            }
            let cfg = config::load_resolved(&inst.config_path())?;
            let (ssh_opts, command) = split_ssh_args(&args.args);
            ssh::session(&inst, &cfg.user, ssh_opts, command).await
        }
        Command::Ls => {
            // Gathered into rows so we can compute column widths after.
            struct Row {
                name: String,
                status: String,
                memory: String,
                cpus: String,
                disk: String,
            }

            let instances = vm::list().await?;
            if instances.is_empty() {
                eprintln!("No VMs found. Create one with: agv create <name>");
                return Ok(());
            }

            let mut rows = Vec::with_capacity(instances.len());
            for inst in &instances {
                let status = inst
                    .reconcile_status()
                    .await
                    .map_or_else(|_| "unknown".to_string(), |s| s.to_string());
                let status = if status == "broken" {
                    let state = inst.read_provision_state().await;
                    format!("broken ({})", vm::broken_substate(&state))
                } else {
                    status
                };
                // Best-effort: show "?" if config can't be read.
                let (memory, cpus, disk_max) =
                    config::load_resolved(&inst.config_path()).map_or_else(
                        |_| ("?".to_string(), "?".to_string(), "?".to_string()),
                        |c| (c.memory, c.cpus.to_string(), c.disk),
                    );
                // Actual on-disk size of the qcow2 file. qcow2 grows as the
                // guest writes, so this is much more useful than the maximum.
                let disk_used = tokio::fs::metadata(inst.disk_path())
                    .await
                    .map_or_else(|_| "?".to_string(), |m| format_size(m.len()));
                let disk = format!("{disk_used}/{disk_max}");
                rows.push(Row {
                    name: inst.name.clone(),
                    status,
                    memory,
                    cpus,
                    disk,
                });
            }

            let name_w = rows.iter().map(|r| r.name.len()).max().unwrap_or(0);
            let status_w = rows.iter().map(|r| r.status.len()).max().unwrap_or(0);
            let mem_w = rows.iter().map(|r| r.memory.len()).max().unwrap_or(0);
            let cpus_w = rows.iter().map(|r| r.cpus.len()).max().unwrap_or(0);
            let disk_w = rows.iter().map(|r| r.disk.len()).max().unwrap_or(0);
            for r in &rows {
                println!(
                    "  {:<name_w$}  {:<status_w$}  {:>mem_w$} RAM  {:>cpus_w$} vCPUs  {:>disk_w$} disk",
                    r.name, r.status, r.memory, r.cpus, r.disk,
                );
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
                let inst = vm::instance::Instance::open(&s.name)?;
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
                    let inst = vm::instance::Instance::open(&s.name)?;
                    config::load_resolved(&inst.config_path())?
                };
                let old_memory = inst_config.memory.clone();
                let old_cpus = inst_config.cpus;
                let old_disk = inst_config.disk.clone();
                let old_forwards = inst_config.forwards.clone();

                vm::config_set(
                    &s.name,
                    s.memory.as_deref(),
                    s.cpus,
                    s.disk.as_deref(),
                    s.forwards.as_deref(),
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
                if s.forwards.is_some() {
                    let new = {
                        let inst = vm::instance::Instance::open(&s.name)?;
                        config::load_resolved(&inst.config_path())?.forwards
                    };
                    let old_fmt = if old_forwards.is_empty() {
                        "(none)".to_string()
                    } else {
                        old_forwards.join(", ")
                    };
                    let new_fmt = if new.is_empty() {
                        "(none)".to_string()
                    } else {
                        new.join(", ")
                    };
                    println!("  forwards: {old_fmt} → {new_fmt}");
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
        Command::Forward(args) => forward_command(args, quiet).await,
        Command::ForwardDaemon(args) => {
            let spec: forward::ForwardSpec = args.spec.parse()?;
            forward_daemon::run(&args.name, spec).await
        }
        Command::Cp(args) => {
            // Validate path syntax before opening the VM.
            let src_is_vm = args.source.starts_with(':');
            let dst_is_vm = args.dest.starts_with(':');
            anyhow::ensure!(
                src_is_vm || dst_is_vm,
                "one of source or dest must be a VM path (prefixed with :)"
            );
            anyhow::ensure!(
                !(src_is_vm && dst_is_vm),
                "cannot copy between two VM paths — one side must be a local path"
            );

            let inst = vm::instance::Instance::open(&args.name)?;
            let status = inst.reconcile_status().await?;
            if status != vm::instance::Status::Running {
                return Err(not_running_error(&args.name, status));
            }
            let cfg = config::load_resolved(&inst.config_path())?;

            ssh::transfer(&inst, &cfg.user, &args.source, &args.dest, args.recursive, verbose)
                .await?;

            if !quiet {
                let direction = if src_is_vm { "downloaded" } else { "uploaded" };
                let local = if src_is_vm { &args.dest } else { &args.source };
                let remote = if src_is_vm { &args.source } else { &args.dest };
                println!("  {direction}: {local} ↔ {}{remote}", args.name);
            }

            Ok(())
        }
        Command::Doctor(args) => {
            if args.setup_ssh {
                return ssh_config::install_include();
            }
            if args.remove_ssh {
                return ssh_config::remove_include();
            }
            doctor::run()?;
            // Check SSH config integration.
            println!();
            match ssh_config::is_include_installed() {
                Ok(true) => println!("  SSH config Include: ✓ installed"),
                Ok(false) => {
                    println!("  SSH config Include: not set up");
                    println!("    Run: agv doctor --setup-ssh");
                    println!("    This lets you ssh into VMs by name (e.g. ssh myvm) and");
                    println!("    enables IDE remote development (VS Code, JetBrains, etc.).");
                }
                Err(_) => {}
            }
            Ok(())
        }
        Command::Init(args) => {
            init::run(args.template.as_deref(), &args.output, args.force)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(s: &str) -> String {
        s.to_string()
    }

    #[test]
    fn split_ssh_args_empty() {
        let (opts, cmd) = split_ssh_args(&[]);
        assert!(opts.is_empty());
        assert!(cmd.is_empty());
    }

    #[test]
    fn split_ssh_args_opts_only() {
        let args = vec![s("-A"), s("-L"), s("8080:localhost:8080")];
        let (opts, cmd) = split_ssh_args(&args);
        assert_eq!(opts, &[s("-A"), s("-L"), s("8080:localhost:8080")]);
        assert!(cmd.is_empty());
    }

    #[test]
    fn split_ssh_args_command_only() {
        let args = vec![s("--"), s("ls"), s("-la")];
        let (opts, cmd) = split_ssh_args(&args);
        assert!(opts.is_empty());
        assert_eq!(cmd, &[s("ls"), s("-la")]);
    }

    #[test]
    fn split_ssh_args_opts_and_command() {
        let args = vec![s("-A"), s("--"), s("ls"), s("-la")];
        let (opts, cmd) = split_ssh_args(&args);
        assert_eq!(opts, &[s("-A")]);
        assert_eq!(cmd, &[s("ls"), s("-la")]);
    }

    #[test]
    fn split_ssh_args_separator_at_start() {
        let args = vec![s("--"), s("ls")];
        let (opts, cmd) = split_ssh_args(&args);
        assert!(opts.is_empty());
        assert_eq!(cmd, &[s("ls")]);
    }

    #[test]
    fn split_ssh_args_separator_at_end() {
        let args = vec![s("-N"), s("--")];
        let (opts, cmd) = split_ssh_args(&args);
        assert_eq!(opts, &[s("-N")]);
        assert!(cmd.is_empty());
    }

    #[test]
    fn format_size_bytes() {
        assert_eq!(format_size(0), "0B");
        assert_eq!(format_size(512), "512B");
    }

    #[test]
    fn format_size_kib() {
        assert_eq!(format_size(1024), "1K");
        assert_eq!(format_size(2 * 1024), "2K");
    }

    #[test]
    fn format_size_mib() {
        assert_eq!(format_size(1024 * 1024), "1M");
        assert_eq!(format_size(512 * 1024 * 1024), "512M");
    }

    #[test]
    fn format_size_gib() {
        assert_eq!(format_size(1024 * 1024 * 1024), "1.0G");
        assert_eq!(format_size(8 * 1024 * 1024 * 1024), "8.0G");
        assert_eq!(format_size(2400 * 1024 * 1024), "2.3G");
    }

    #[test]
    fn format_size_tib() {
        assert_eq!(format_size(1024_u64.pow(4)), "1.0T");
    }
}
