//! agv — create and manage QEMU VMs for AI agents.
//!
//! This crate is the implementation of the `agv` CLI. It is **not** intended
//! for use as a library — the public module surface exists so the CLI binary
//! (`src/main.rs`) and the integration tests under `tests/` can reach
//! internals. Items are marked `#[doc(hidden)]` to make that intent
//! explicit on docs.rs.
//!
//! No semver guarantees are offered for anything other than the CLI behavior
//! itself. If you have a library use case, please open an issue so we can
//! shape an intentional public API rather than leaking internals.

#[doc(hidden)]
pub mod cli;
#[doc(hidden)]
pub mod config;
#[doc(hidden)]
pub mod dirs;
#[doc(hidden)]
pub mod doctor;
#[doc(hidden)]
pub mod error;
#[doc(hidden)]
pub mod gui;
#[doc(hidden)]
pub mod forward;
#[doc(hidden)]
pub mod forward_daemon;
#[doc(hidden)]
pub mod image;
#[doc(hidden)]
pub mod images;
#[doc(hidden)]
pub mod init;
#[doc(hidden)]
pub mod interactive;
#[doc(hidden)]
pub mod locks;
#[doc(hidden)]
pub mod manual_steps;
#[doc(hidden)]
pub mod resources;
#[doc(hidden)]
pub mod specs;
#[doc(hidden)]
pub mod ssh;
#[doc(hidden)]
pub mod ssh_config;
#[doc(hidden)]
pub mod template;
#[doc(hidden)]
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

/// Split `agv ssh` trailing args into `(ssh_opts, remote_command)`.
///
/// Routing rules, in order:
/// 1. If `args` contains `--`, split there. (Happens when the user
///    passed e.g. `-A -- ls`: clap preserves the `--` once at least
///    one non-`--` value precedes it.)
/// 2. Otherwise, check whether the user typed `--` immediately after
///    the VM name on the *raw* command line. clap's `trailing_var_arg`
///    silently consumes a *leading* `--`, so without this check
///    `agv ssh myvm -- ls` would parse as `args = ["ls"]` and our
///    function would mistakenly treat `ls` as an ssh option (which
///    ssh then tries to use as a hostname). When that pattern is
///    detected, every captured arg is the remote command.
/// 3. Else, no `--` was involved at all — treat everything as ssh
///    options (interactive session with extra flags, or no args at
///    all).
fn split_ssh_args<'a>(name: &str, args: &'a [String]) -> (&'a [String], &'a [String]) {
    if let Some(i) = args.iter().position(|a| a == "--") {
        return (&args[..i], &args[i + 1..]);
    }
    if raw_argv_has_leading_dash_dash_after_ssh(name) {
        return (&[], args);
    }
    (args, &[])
}

/// Did the user type `--` immediately after `agv ssh <name>` on the
/// shell? Walks `std::env::args_os()` to recover what clap discarded.
fn raw_argv_has_leading_dash_dash_after_ssh(name: &str) -> bool {
    has_leading_dash_dash_after_ssh(std::env::args_os(), name)
}

/// Pure version of [`raw_argv_has_leading_dash_dash_after_ssh`] —
/// takes an iterator of argv tokens so it's directly testable
/// without spawning a subprocess.
fn has_leading_dash_dash_after_ssh<I>(argv: I, name: &str) -> bool
where
    I: IntoIterator,
    I::Item: AsRef<std::ffi::OsStr>,
{
    let mut iter = argv.into_iter();
    // Skip until the literal "ssh" subcommand. Global flags
    // (`--quiet`, `--verbose`) don't share that name.
    let mut found_ssh = false;
    for arg in iter.by_ref() {
        if arg.as_ref() == "ssh" {
            found_ssh = true;
            break;
        }
    }
    if !found_ssh {
        return false;
    }
    // Skip until the VM name. Defensive against future flags
    // sitting between `ssh` and the positional name.
    let mut found_name = false;
    for arg in iter.by_ref() {
        if arg.as_ref() == name {
            found_name = true;
            break;
        }
    }
    if !found_name {
        return false;
    }
    iter.next().is_some_and(|a| a.as_ref() == "--")
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

/// Render a labels map as a single-line `k=v, k=v` string for inline
/// display in tables. Empty map produces an empty string (no visual
/// noise on rows that have no labels).
fn format_labels_inline(labels: &std::collections::BTreeMap<String, String>) -> String {
    labels
        .iter()
        .map(|(k, v)| if v.is_empty() { k.clone() } else { format!("{k}={v}") })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Match an instance against a set of `key=value` (or bare-key) selectors.
/// Returns true when every selector matches; bare key matches any value.
fn instance_matches_labels(
    inst_labels: &std::collections::BTreeMap<String, String>,
    selectors: &std::collections::BTreeMap<String, Option<String>>,
) -> bool {
    selectors.iter().all(|(k, want)| match (want, inst_labels.get(k)) {
        (None, Some(_)) => true, // bare-key selector matches any value
        (Some(want_v), Some(have_v)) => want_v == have_v,
        _ => false,
    })
}

/// Parse `--label k=v` selectors used by `agv ls` / `agv destroy`. The
/// shape differs slightly from `config::parse_labels` (used by `agv
/// create`): a bare key without `=` is a "key exists" wildcard rather
/// than `key=""`. Duplicate selectors collapse (the OR of "I want this
/// value" and "any value" is "any value", so the bare form wins).
fn parse_label_selectors(
    raw: &[String],
) -> anyhow::Result<std::collections::BTreeMap<String, Option<String>>> {
    let mut out: std::collections::BTreeMap<String, Option<String>> =
        std::collections::BTreeMap::new();
    for entry in raw {
        let (key, want) = match entry.split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v.to_string())),
            None => (entry.clone(), None),
        };
        if key.is_empty() {
            anyhow::bail!("invalid --label {entry:?}: key cannot be empty");
        }
        // Bare-key wildcard wins over an exact-value selector for the same
        // key (more permissive).
        if !matches!(out.get(&key), Some(None)) {
            out.insert(key, want);
        }
    }
    Ok(out)
}

/// Implementation for `agv destroy`. Handles both single-VM (positional
/// name) and bulk-by-label (`--label k=v`) modes. clap's `conflicts_with`
/// already prevents both being set; here we also reject the case where
/// neither is set.
async fn destroy_command(args: &cli::DestroyArgs, yes: bool) -> anyhow::Result<()> {
    if args.name.is_none() && args.label.is_empty() {
        anyhow::bail!("agv destroy requires either a VM name or --label <k=v>");
    }

    if let Some(name) = &args.name {
        // Single-VM path — same behaviour as before, just with the
        // structured DestroyReport on --json.
        tracing::info!(name = %name, force = args.force, "destroying VM");
        vm::destroy(name, args.force).await?;
        if args.json {
            let report = vm::DestroyReport {
                name: name.clone(),
                destroyed: true,
            };
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            println!("  ✓ VM '{name}' destroyed");
        }
        return Ok(());
    }

    // Bulk path: enumerate VMs, filter by label selectors, list, prompt,
    // destroy.
    let selectors = parse_label_selectors(&args.label)?;
    let instances = vm::list().await?;
    let mut matches = Vec::new();
    for inst in &instances {
        let Ok(cfg) = config::load_resolved(&inst.config_path()) else {
            continue;
        };
        if instance_matches_labels(&cfg.labels, &selectors) {
            let status = inst
                .reconcile_status()
                .await
                .unwrap_or(vm::instance::Status::Stopped);
            matches.push((inst.name.clone(), status));
        }
    }

    if matches.is_empty() {
        if args.json {
            println!("[]");
        } else {
            eprintln!("No VMs match the given label selectors.");
        }
        return Ok(());
    }

    // Refuse running VMs unless --force.
    if !args.force {
        let running: Vec<&String> = matches
            .iter()
            .filter(|(_, s)| matches!(s, vm::instance::Status::Running | vm::instance::Status::Configuring))
            .map(|(n, _)| n)
            .collect();
        if !running.is_empty() {
            anyhow::bail!(
                "{} matched VM(s) are running ({}); pass --force to tear them down anyway",
                running.len(),
                running.iter().map(|n| n.as_str()).collect::<Vec<_>>().join(", "),
            );
        }
    }

    // Confirmation prompt unless -y.
    if !yes && !args.json {
        use std::io::Write as _;
        eprintln!("Will destroy {} VM(s):", matches.len());
        for (n, s) in &matches {
            eprintln!("  - {n}  ({s})");
        }
        eprintln!();
        eprint!("Continue? [y/N] ");
        std::io::stderr().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        let answer = answer.trim().to_lowercase();
        if answer != "y" && answer != "yes" {
            eprintln!("Aborted.");
            return Ok(());
        }
    }

    // Destroy each. Collect reports for --json output.
    let mut reports = Vec::with_capacity(matches.len());
    for (name, _) in &matches {
        tracing::info!(name = %name, force = args.force, "bulk destroying VM");
        match vm::destroy(name, args.force).await {
            Ok(()) => {
                if args.json {
                    reports.push(vm::DestroyReport {
                        name: name.clone(),
                        destroyed: true,
                    });
                } else {
                    println!("  ✓ VM '{name}' destroyed");
                }
            }
            Err(e) => {
                // One failure shouldn't abort the rest of the bulk action.
                // Report it and keep going so the user gets a clean state at
                // the end (or as clean as we can manage).
                eprintln!("  ✗ failed to destroy '{name}': {e:#}");
            }
        }
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&reports)?);
    }
    Ok(())
}

/// Open the named instance, build a `VmStateReport` (with `created: false`
/// since by definition we're past the create boundary), and print it as
/// pretty-formatted JSON. Used by every lifecycle verb that has a `--json`
/// flag (start, stop, suspend, resume, rename) — destroy doesn't go
/// through here because the VM no longer exists.
async fn emit_state_report(name: &str) -> anyhow::Result<()> {
    let inst = vm::instance::Instance::open(name)?;
    let report = vm::state_report(&inst, false).await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

/// Implementation for `agv resources`.
///
/// Two short blocks: host capacity, and what agv has currently allocated
/// (sum across running VMs, sum across every known VM). `--json` emits a
/// `ResourceReport` directly so agents can parse it without scraping.
async fn print_resources(json: bool) -> anyhow::Result<()> {
    let report = resources::report().await?;
    if json {
        let out = serde_json::to_string_pretty(&report)?;
        println!("{out}");
        return Ok(());
    }

    let host = &report.host;
    let alloc = &report.allocated;
    println!("Host:");
    println!(
        "  RAM         {} used of {} total",
        format_size(host.used_memory_bytes),
        format_size(host.total_memory_bytes),
    );
    println!("  CPUs        {}", host.cpus);
    println!(
        "  Data dir    {} free",
        format_size(host.data_dir_free_bytes)
    );
    println!();
    println!("Allocated to agv VMs:");
    println!(
        "  Running     {} RAM · {} vCPUs · {} VM(s)",
        format_size(alloc.running_memory_bytes),
        alloc.running_cpus,
        alloc.running_count,
    );
    println!(
        "  Total       {} RAM · {} vCPUs · {} disk · {} VM(s)",
        format_size(alloc.total_memory_bytes),
        alloc.total_cpus,
        format_size(alloc.total_disk_bytes),
        alloc.total_count,
    );
    Ok(())
}

async fn forward_command(args: cli::ForwardArgs, quiet: bool) -> anyhow::Result<()> {
    if args.list {
        let active = vm::forwarding::list(&args.name).await?;
        if args.json {
            let entries: Vec<forward::ForwardJson> =
                active.iter().copied().map(Into::into).collect();
            println!("{}", serde_json::to_string_pretty(&entries)?);
            return Ok(());
        }
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
                "  host:{host:>w$} {arrow} VM:{guest} ({origin})",
                host = a.host,
                w = host_width,
                guest = a.guest,
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
                "  ✓ host:{host} {arrow} VM:{guest}",
                host = entry.host,
                guest = entry.guest,
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
#[doc(hidden)]
pub async fn run(cli: Cli) -> anyhow::Result<()> {
    dirs::ensure_dirs().await?;

    let verbose = cli.verbose;
    let quiet = cli.quiet;

    match cli.command {
        Command::Create(args) => {
            let start = args.start;
            let interactive = args.interactive;
            let force = args.force;
            let if_not_exists = args.if_not_exists;
            let json = args.json;
            let name = args.name.clone();
            if interactive && args.from.is_some() {
                anyhow::bail!("--interactive cannot be combined with --from (template clones do not run provisioning)");
            }

            // --if-not-exists: short-circuit when the VM is already there.
            // Both create-from-template and create-from-config paths honor it.
            if if_not_exists && dirs::instance_dir(&name)?.exists() {
                let inst = vm::instance::Instance::open(&name)?;
                let report = vm::state_report(&inst, false).await?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&report)?);
                } else {
                    println!("VM '{name}' already exists (status: {}). No changes.", report.status);
                }
                return Ok(());
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
                .await?;
            } else {
                let config = config::build_from_cli(&args)?;
                tracing::info!(name = %name, "creating VM");
                vm::create(&name, &config, start, interactive, verbose, quiet, force).await?;
            }

            // Post-create handoff: when --json was passed, emit a parseable
            // state object so an agent can act without a follow-up `inspect`.
            if json {
                let inst = vm::instance::Instance::open(&name)?;
                let report = vm::state_report(&inst, true).await?;
                println!("{}", serde_json::to_string_pretty(&report)?);
            }
            Ok(())
        }
        Command::Start(args) => {
            tracing::info!(name = %args.name, retry = args.retry, "starting VM");
            // --json implies suppressing progress chrome so JSON parsing
            // isn't broken by spinner residue or "step done" lines.
            let effective_quiet = quiet || args.json;
            vm::start(&args.name, args.retry, args.interactive, verbose, effective_quiet).await?;
            if args.json {
                emit_state_report(&args.name).await?;
            }
            Ok(())
        }
        Command::Stop(args) => {
            tracing::info!(name = %args.name, force = args.force, "stopping VM");
            vm::stop(&args.name, args.force).await?;
            if args.json {
                emit_state_report(&args.name).await?;
            }
            Ok(())
        }
        Command::Suspend(args) => {
            tracing::info!(name = %args.name, "suspending VM");
            vm::suspend(&args.name).await?;
            if args.json {
                emit_state_report(&args.name).await?;
            } else {
                println!("  ✓ VM '{}' suspended", args.name);
            }
            Ok(())
        }
        Command::Resume(args) => {
            tracing::info!(name = %args.name, "resuming VM");
            let effective_quiet = quiet || args.json;
            vm::resume(&args.name, verbose, effective_quiet).await?;
            if args.json {
                emit_state_report(&args.name).await?;
            }
            Ok(())
        }
        Command::Destroy(args) => destroy_command(&args, cli.yes).await,
        Command::Rename(args) => {
            tracing::info!(old = %args.old, new = %args.new, "renaming VM");
            vm::rename(&args.old, &args.new).await?;
            if args.json {
                emit_state_report(&args.new).await?;
            } else {
                println!("  ✓ VM '{}' renamed to '{}'", args.old, args.new);
                println!();
                println!("  Note: the hostname inside the guest is unchanged.");
                println!("  To update it, SSH in after starting the VM and run:");
                println!("    sudo hostnamectl set-hostname {}", args.new);
            }
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
            let (ssh_opts, command) = split_ssh_args(&args.name, &args.args);
            ssh::session(&inst, &cfg.user, ssh_opts, command).await
        }
        Command::Gui(args) => gui::run(&args.name, args.no_launch).await,
        Command::Ls(args) => {
            // Gathered into rows so we can compute column widths after.
            struct Row {
                name: String,
                status: String,
                memory: String,
                cpus: String,
                disk: String,
                labels: String,
            }

            // Pre-parse label selectors once. Empty Vec → empty selector
            // map → matches every VM (instance_matches_labels returns true
            // when all of zero selectors match).
            let selectors = parse_label_selectors(&args.label)?;

            let all_instances = vm::list().await?;

            // Filter by label selector when one is given. Best-effort: a
            // VM whose saved config can't load is excluded from labelled
            // queries (we have no labels to match against).
            let instances: Vec<_> = if selectors.is_empty() {
                all_instances
            } else {
                let mut filtered = Vec::new();
                for inst in all_instances {
                    let Ok(cfg) = config::load_resolved(&inst.config_path()) else {
                        continue;
                    };
                    if instance_matches_labels(&cfg.labels, &selectors) {
                        filtered.push(inst);
                    }
                }
                filtered
            };

            if args.json {
                // VmStateReport for every instance — best-effort: VMs whose
                // saved config can't load (e.g. mid-create crash) are
                // skipped, with a debug trace that surfaces under -v.
                let mut reports = Vec::with_capacity(instances.len());
                for inst in &instances {
                    match vm::state_report(inst, false).await {
                        Ok(r) => reports.push(r),
                        Err(e) => {
                            tracing::debug!(
                                vm = %inst.name,
                                error = %format!("{e:#}"),
                                "failed to build state_report for ls --json; skipping"
                            );
                        }
                    }
                }
                println!("{}", serde_json::to_string_pretty(&reports)?);
                return Ok(());
            }

            if instances.is_empty() {
                if args.label.is_empty() {
                    eprintln!("No VMs found. Create one with: agv create <name>");
                } else {
                    eprintln!("No VMs match the given label selectors.");
                }
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
                // Best-effort: show "?" if config can't be read, but leave
                // a debug trace so `agv -v ls` surfaces the parse/IO error
                // instead of silently hiding it behind "?".
                let (memory, cpus, disk_max, labels_str) =
                    match config::load_resolved(&inst.config_path()) {
                        Ok(c) => {
                            let labels_str = format_labels_inline(&c.labels);
                            (c.memory, c.cpus.to_string(), c.disk, labels_str)
                        }
                        Err(e) => {
                            tracing::debug!(
                                vm = %inst.name,
                                error = %format!("{e:#}"),
                                "failed to read instance config for ls row"
                            );
                            ("?".to_string(), "?".to_string(), "?".to_string(), String::new())
                        }
                    };
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
                    labels: labels_str,
                });
            }

            let name_w = rows.iter().map(|r| r.name.len()).max().unwrap_or(0);
            let status_w = rows.iter().map(|r| r.status.len()).max().unwrap_or(0);
            let mem_w = rows.iter().map(|r| r.memory.len()).max().unwrap_or(0);
            let cpus_w = rows.iter().map(|r| r.cpus.len()).max().unwrap_or(0);
            let disk_w = rows.iter().map(|r| r.disk.len()).max().unwrap_or(0);
            for r in &rows {
                if args.labels {
                    println!(
                        "  {:<name_w$}  {:<status_w$}  {:>mem_w$} RAM  {:>cpus_w$} vCPUs  {:>disk_w$} disk  {labels}",
                        r.name, r.status, r.memory, r.cpus, r.disk,
                        labels = r.labels,
                    );
                } else {
                    println!(
                        "  {:<name_w$}  {:<status_w$}  {:>mem_w$} RAM  {:>cpus_w$} vCPUs  {:>disk_w$} disk",
                        r.name, r.status, r.memory, r.cpus, r.disk,
                    );
                }
            }
            Ok(())
        }
        Command::Images(args) => {
            let all = images::list_all()?;
            if args.json {
                let entries: Vec<images::ImageJson> =
                    all.iter().map(images::ImageJson::from).collect();
                println!("{}", serde_json::to_string_pretty(&entries)?);
                return Ok(());
            }
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
            if args.json {
                let inst = vm::instance::Instance::open(&args.name)?;
                let report = vm::state_report(&inst, false).await?;
                println!("{}", serde_json::to_string_pretty(&report)?);
                Ok(())
            } else {
                vm::inspect(&args.name).await
            }
        }
        Command::Cache(args) => match args.command {
            CacheCommand::Ls(largs) => {
                let entries = image::list_cache().await?;
                if largs.json {
                    println!("{}", serde_json::to_string_pretty(&entries)?);
                    return Ok(());
                }
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
        Command::Specs(args) => {
            let all = specs::list_all()?;
            if args.json {
                let entries: Vec<specs::SpecJson> =
                    all.iter().map(specs::SpecJson::from).collect();
                println!("{}", serde_json::to_string_pretty(&entries)?);
                return Ok(());
            }
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
        Command::Resources(args) => print_resources(args.json).await,
        Command::Template(args) => match args.command {
            TemplateCommand::Create(targs) => {
                tracing::info!(
                    vm = %targs.vm,
                    template = %targs.name,
                    "creating template"
                );
                vm::create_template(&targs.vm, &targs.name, targs.stop, verbose, quiet).await
            }
            TemplateCommand::Ls(targs) => {
                let templates = vm::list_templates().await?;
                if targs.json {
                    println!("{}", serde_json::to_string_pretty(&templates)?);
                    return Ok(());
                }
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
            if args.json {
                return doctor::run_json();
            }
            doctor::run()

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

    /// When `args` contains an explicit `--` (clap-preserved
    /// case — at least one value preceded it), split there. The
    /// raw-argv fallback isn't exercised in these tests because
    /// `std::env::args_os()` reflects the test binary's own argv
    /// at the time the test runs.
    #[test]
    fn split_ssh_args_separator_in_middle() {
        let args = [s("-A"), s("--"), s("ls"), s("-la")];
        let (opts, cmd) = split_ssh_args("myvm", &args);
        assert_eq!(opts, &[s("-A")]);
        assert_eq!(cmd, &[s("ls"), s("-la")]);
    }

    #[test]
    fn split_ssh_args_separator_at_start_in_args() {
        let args = [s("--"), s("ls")];
        let (opts, cmd) = split_ssh_args("myvm", &args);
        assert!(opts.is_empty());
        assert_eq!(cmd, &[s("ls")]);
    }

    #[test]
    fn split_ssh_args_separator_at_end() {
        let args = [s("-N"), s("--")];
        let (opts, cmd) = split_ssh_args("myvm", &args);
        assert_eq!(opts, &[s("-N")]);
        assert!(cmd.is_empty());
    }

    /// No `--` and no leading `--` in raw argv — everything is
    /// ssh options.
    #[test]
    fn split_ssh_args_only_opts() {
        let args = [s("-A"), s("-L"), s("8080:localhost:8080")];
        let (opts, cmd) = split_ssh_args("myvm", &args);
        assert_eq!(opts, &[s("-A"), s("-L"), s("8080:localhost:8080")]);
        assert!(cmd.is_empty());
    }

    #[test]
    fn split_ssh_args_empty() {
        let (opts, cmd) = split_ssh_args("myvm", &[]);
        assert!(opts.is_empty());
        assert!(cmd.is_empty());
    }

    /// `has_leading_dash_dash_after_ssh` is the recovery path for
    /// the case where clap eats `--`. It scans raw argv looking for
    /// `agv ssh <name> --`.
    #[test]
    fn detects_dash_dash_after_vm_name() {
        let argv = ["agv", "ssh", "myvm", "--", "cat", "foo"];
        assert!(has_leading_dash_dash_after_ssh(argv, "myvm"));
    }

    #[test]
    fn detects_dash_dash_after_global_flags() {
        let argv = ["agv", "--quiet", "ssh", "myvm", "--", "cat"];
        assert!(has_leading_dash_dash_after_ssh(argv, "myvm"));
    }

    #[test]
    fn no_dash_dash_when_value_precedes_it() {
        let argv = ["agv", "ssh", "myvm", "-A", "--", "ls"];
        assert!(!has_leading_dash_dash_after_ssh(argv, "myvm"));
    }

    #[test]
    fn no_dash_dash_for_bare_interactive() {
        let argv = ["agv", "ssh", "myvm"];
        assert!(!has_leading_dash_dash_after_ssh(argv, "myvm"));
    }

    /// VM names that collide with the `ssh` subcommand string
    /// itself shouldn't fool the scanner. Skipping the first match
    /// of "ssh" lands us at the subcommand; the name search
    /// continues from there.
    #[test]
    fn vm_name_named_ssh_still_works() {
        let argv = ["agv", "ssh", "ssh", "--", "cat"];
        assert!(has_leading_dash_dash_after_ssh(argv, "ssh"));
    }

    /// Different subcommand — the scanner doesn't trigger.
    #[test]
    fn ignores_dash_dash_in_other_subcommands() {
        let argv = ["agv", "ls", "--", "myvm"];
        assert!(!has_leading_dash_dash_after_ssh(argv, "myvm"));
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
