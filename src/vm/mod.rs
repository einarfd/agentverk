//! VM lifecycle management — create, start, stop, destroy.
//!
//! This module orchestrates the high-level VM operations, delegating to
//! submodules for QEMU process management, cloud-init, and instance state.

pub mod cloud_init;
pub mod forwarding;
pub mod instance;
pub mod provision;
pub mod qemu;
pub mod template;

// Re-export template CRUD at `vm::*` so call sites in `lib.rs` keep using
// `vm::create_template`, `vm::list_templates`, etc.
pub use template::{
    create_from_template, create_template, list_templates, remove_template, TemplateInfo,
};

// Local aliases so existing call sites in this module can keep calling
// `run_first_boot(...)` and `wait_for_ssh(...)` unchanged after the move.
use provision::{run_first_boot, wait_for_ssh};

use std::io::IsTerminal as _;
use std::time::Duration;

use anyhow::Context as _;
use indicatif::ProgressBar;
use tracing::{debug, info, warn};

use crate::config::ResolvedConfig;
use crate::error::Error;
use crate::{dirs, image, ssh, ssh_config};
use instance::{Instance, Phase, ProvisionState, Status};

/// Create an indicatif spinner for status messages.
///
/// Returns a hidden (no-op) bar when `quiet` is set or stderr is not a TTY
/// (and `verbose` is not set). With `verbose`, always shows status.
pub(super) fn status_spinner(verbose: bool, quiet: bool) -> ProgressBar {
    if quiet {
        return ProgressBar::hidden();
    }
    if verbose || std::io::stderr().is_terminal() {
        let pb = ProgressBar::new_spinner();
        pb.enable_steady_tick(Duration::from_millis(100));
        pb
    } else {
        ProgressBar::hidden()
    }
}

/// Update the managed SSH config with this VM's connection details.
///
/// Best-effort — failures are logged but do not abort the operation.
async fn update_ssh_config(inst: &Instance, user: &str) {
    let port = match tokio::fs::read_to_string(inst.ssh_port_path()).await {
        Ok(raw) => match raw.trim().parse::<u16>() {
            Ok(p) => p,
            Err(_) => return,
        },
        Err(_) => return,
    };
    if let Err(e) = ssh_config::add_entry(&inst.name, port, user, &inst.ssh_key_path()).await {
        warn!(vm = %inst.name, error = %format!("{e:#}"), "failed to update managed SSH config");
    }
}

/// Print a completed-step line above the spinner, keeping previous output visible.
pub(super) fn step_done(spinner: &ProgressBar, msg: &str) {
    spinner.println(format!("  ✓ {msg}"));
}

/// Apply config-declared forwards to a newly-started VM and surface results.
///
/// Failures are non-fatal: the VM is already up, so a port collision should
/// not mark it broken. Each failed spec is reported inline so the user can
/// act on it (edit config, free the port) without having to re-check status.
async fn apply_and_report_forwards(
    inst: &Instance,
    config: &ResolvedConfig,
    spinner: &ProgressBar,
) {
    // Config forwards first (resets the state file for this boot).
    if config.forwards.is_empty() {
        // Still clear any stale state left from a previous boot.
        if let Err(e) = crate::forward::clear_active(&inst.forwards_path()).await {
            debug!(vm = %inst.name, error = %format!("{e:#}"), "failed to clear stale forwards state");
        }
    } else {
        let specs = match crate::forward::parse_specs(config.forwards.iter()) {
            Ok(s) => s,
            Err(e) => {
                spinner.println(format!(
                    "  ! Skipping forwards — failed to parse config: {e:#}"
                ));
                return;
            }
        };
        match forwarding::apply_config_forwards(inst, &specs).await {
            Ok(outcome) => {
                if !outcome.applied.is_empty() {
                    step_done(
                        spinner,
                        &format!(
                            "Applied {} forward{}",
                            outcome.applied.len(),
                            if outcome.applied.len() == 1 { "" } else { "s" }
                        ),
                    );
                }
                for (spec, msg) in &outcome.failures {
                    spinner.println(format!("  ! Forward {spec} failed: {msg}"));
                }
            }
            Err(e) => {
                spinner.println(format!(
                    "  ! Failed to persist forwards state: {e:#}"
                ));
            }
        }
    }

    // Auto-allocated forwards — mixins' named tunnels (e.g. RDP, VNC).
    // Runs after config forwards so they share one forwards.toml state
    // file that's cleanly reset at the start of each boot.
    if !config.auto_forwards.is_empty() {
        match forwarding::apply_auto_forwards(inst, &config.auto_forwards).await {
            Ok(outcome) => {
                for (name, entry) in &outcome.applied {
                    step_done(
                        spinner,
                        &format!(
                            "Auto-forward {name}: 127.0.0.1:{} → guest:{}",
                            entry.host, entry.guest
                        ),
                    );
                }
                for (name, msg) in &outcome.failures {
                    spinner.println(format!("  ! Auto-forward {name} failed: {msg}"));
                }
            }
            Err(e) => {
                spinner.println(format!(
                    "  ! Failed to apply auto-forwards: {e:#}"
                ));
            }
        }
    }
}

/// Mark a VM as broken and persist the error to all the relevant places.
///
/// Used by both `create()` and `start()` when first-boot provisioning fails.
/// Updates: status → broken, `error.log`, `provision_state.error`.
async fn mark_broken_with_error(inst: &Instance, error: &anyhow::Error) {
    let msg = format!("{error:#}");
    if let Err(e) = inst.write_status(Status::Broken).await {
        warn!(vm = %inst.name, error = %format!("{e:#}"), "failed to persist broken status");
    }
    if let Err(e) = tokio::fs::write(inst.error_log_path(), &msg).await {
        warn!(vm = %inst.name, error = %format!("{e:#}"), "failed to write error.log");
    }
    let mut state = inst.read_provision_state().await;
    state.error = Some(msg);
    if let Err(e) = inst.write_provision_state(&state).await {
        warn!(vm = %inst.name, error = %format!("{e:#}"), "failed to persist provision_state");
    }
}

/// Create a new VM from the given resolved configuration.
///
/// This is the top-level entry point with error recovery: if creation fails
/// after the instance directory has been created, the VM is marked as broken
/// and the error is logged to `error.log`.
#[expect(
    clippy::fn_params_excessive_bools,
    reason = "distinct independent flags; bundling them in a struct would push boilerplate to call sites"
)]
pub async fn create(
    name: &str,
    config: &ResolvedConfig,
    start_after: bool,
    interactive_mode: bool,
    verbose: bool,
    quiet: bool,
) -> anyhow::Result<()> {
    // Guard: instance must not already exist.
    let inst_dir = dirs::instance_dir(name)?;
    if inst_dir.exists() {
        return Err(Error::VmAlreadyExists {
            name: name.to_string(),
        }
        .into());
    }

    // Create the instance directory.
    tokio::fs::create_dir_all(&inst_dir)
        .await
        .with_context(|| format!("failed to create instance directory for VM '{name}'"))?;

    let inst = Instance {
        name: name.to_string(),
        dir: inst_dir,
    };

    // Write initial status.
    inst.write_status(Status::Creating).await?;

    // Delegate to inner function; catch errors to mark broken.
    if let Err(e) = create_inner(&inst, name, config, start_after, interactive_mode, verbose, quiet).await {
        // Mark as broken so users can inspect / destroy. Leave QEMU running
        // if it's alive — the user can SSH in to debug.
        mark_broken_with_error(&inst, &e).await;
        return Err(e);
    }

    Ok(())
}

/// Inner creation logic — does all real work, uses `?` for early return.
#[expect(
    clippy::fn_params_excessive_bools,
    reason = "distinct independent flags; bundling them in a struct would push boilerplate to call sites"
)]
async fn create_inner(
    inst: &Instance,
    name: &str,
    config: &ResolvedConfig,
    start_after: bool,
    interactive_mode: bool,
    verbose: bool,
    quiet: bool,
) -> anyhow::Result<()> {
    let spinner = status_spinner(verbose, quiet);

    // Save resolved config to instance dir so restarts / inspect can reload it.
    crate::config::save(config, &inst.config_path()).await?;

    // Derive a short image label from the URL for display.
    let image_label = config
        .base_url
        .rsplit('/')
        .next()
        .unwrap_or(&config.base_url);

    // Cache base image (potentially downloads 500+ MB, idempotent).
    spinner.set_message(format!("Checking base image ({image_label})..."));
    info!(url = %config.base_url, "caching base image");
    let checksum = if config.skip_checksum {
        None
    } else {
        Some(config.base_checksum.as_str())
    };
    let (base_image, downloaded) = image::ensure_cached(&config.base_url, checksum).await?;
    if downloaded {
        step_done(&spinner, &format!("Downloaded base image ({image_label})"));
    } else {
        step_done(&spinner, &format!("Base image cached ({image_label})"));
    }

    // Create qcow2 overlay disk.
    spinner.set_message(format!("Creating {} disk overlay...", config.disk));
    info!(size = %config.disk, "creating overlay disk");
    image::create_overlay(&base_image, &inst.disk_path(), &config.disk).await?;
    step_done(&spinner, &format!("Created {} disk overlay", config.disk));

    // Generate SSH keypair.
    spinner.set_message("Generating SSH keypair...");
    let pub_key = ssh::generate_keypair(inst).await?;
    step_done(&spinner, "Generated SSH keypair");

    // Generate cloud-init seed ISO.
    spinner.set_message("Generating cloud-init seed...");
    info!("generating cloud-init seed ISO");
    cloud_init::generate_seed(&inst.seed_path(), &pub_key, name, &config.user).await?;
    step_done(&spinner, "Generated cloud-init seed");

    // If not starting, we're done — write stopped status.
    if !start_after {
        inst.write_status(Status::Stopped).await?;
        spinner.finish_with_message(format!("  ✓ VM '{name}' created (stopped)"));
        info!(name, "VM created (stopped)");
        return Ok(());
    }

    // Start QEMU.
    spinner.set_message(format!(
        "Starting QEMU ({} RAM, {} vCPUs)...",
        config.memory, config.cpus
    ));
    info!(name, memory = %config.memory, cpus = config.cpus, "starting QEMU");
    qemu::start(inst, &config.memory, config.cpus).await?;
    inst.write_status(Status::Running).await?;
    step_done(
        &spinner,
        &format!("Started QEMU ({} RAM, {} vCPUs)", config.memory, config.cpus),
    );

    // Run first-boot provisioning (wait for SSH, setup, provision).
    run_first_boot(inst, config, interactive_mode, verbose, quiet, &spinner).await?;

    // Apply config-declared and auto-allocated forwards. Must run after
    // SSH is up (the supervisors tunnel through sshd). Same step runs in
    // `start` and `resume` — keeping it here means `agv create --start`
    // yields a VM with its forwards already live.
    apply_and_report_forwards(inst, config, &spinner).await;

    // Update managed SSH config so IDEs can connect by VM name.
    update_ssh_config(inst, &config.user).await;

    spinner.finish_with_message(format!("  ✓ VM '{name}' is running"));
    info!(name, "VM created and running");
    Ok(())
}

/// Change hardware settings of a stopped (or broken) VM.
///
/// Sets the VM to `configuring` status for the duration of the operation so
/// that concurrent `start` calls are safely rejected. Disk resize (grow-only)
/// is performed via `qemu-img resize`; the guest filesystem is not touched.
pub async fn config_set(
    name: &str,
    memory: Option<&str>,
    cpus: Option<u32>,
    disk: Option<&str>,
    forwards: Option<&str>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        memory.is_some() || cpus.is_some() || disk.is_some() || forwards.is_some(),
        "no changes specified — provide at least one of --memory, --cpus, --disk, --forwards"
    );

    let inst = Instance::open(name)?;
    let status = inst.reconcile_status().await?;

    anyhow::ensure!(
        matches!(status, Status::Stopped | Status::Broken),
        Error::VmBadState {
            name: name.to_string(),
            status: status.to_string(),
            expected: "stopped or broken".to_string(),
        }
    );

    let mut config = crate::config::load_resolved(&inst.config_path())?;

    // Validate disk grow-only before touching anything.
    if let Some(new_disk) = disk {
        let current_bytes = image::parse_disk_size(&config.disk)?;
        let new_bytes = image::parse_disk_size(new_disk)?;
        anyhow::ensure!(
            new_bytes > current_bytes,
            "disk can only be grown, not shrunk (current: {}, requested: {})",
            config.disk,
            new_disk
        );
    }

    inst.write_status(Status::Configuring).await?;

    // Resize disk first — qemu-img is atomic; on failure the disk is unchanged.
    if let Some(new_disk) = disk {
        if let Err(e) = image::resize_disk(&inst.disk_path(), new_disk).await {
            let _ = inst.write_status(status).await;
            return Err(e);
        }
        config.disk = image::normalize_size(new_disk)?;
    }

    if let Some(mem) = memory {
        config.memory = image::normalize_size(mem)?;
    }
    if let Some(n) = cpus {
        config.cpus = n;
    }
    if let Some(raw) = forwards {
        let items: Vec<&str> = if raw.trim().is_empty() {
            Vec::new()
        } else {
            raw.split(',').map(str::trim).filter(|s| !s.is_empty()).collect()
        };
        let specs = crate::forward::parse_specs(items)
            .context("invalid --forwards value")?;
        crate::forward::validate_unique(&specs)
            .context("invalid --forwards value")?;
        config.forwards = specs.iter().map(ToString::to_string).collect();
    }

    // Save config; if this fails after a disk resize the state is inconsistent.
    if let Err(e) = crate::config::save(&config, &inst.config_path()).await {
        if disk.is_some() {
            let _ = inst.write_status(Status::Broken).await;
        } else {
            let _ = inst.write_status(status).await;
        }
        return Err(e);
    }

    inst.write_status(Status::Stopped).await?;
    Ok(())
}

/// Start an existing stopped VM.
///
/// If the VM has never been provisioned, runs the full provisioning flow
/// (wait for SSH, setup steps, provision steps) after starting QEMU.
#[expect(
    clippy::fn_params_excessive_bools,
    reason = "distinct independent flags; bundling them in a struct would push boilerplate to call sites"
)]
pub async fn start(
    name: &str,
    retry: bool,
    interactive_mode: bool,
    verbose: bool,
    quiet: bool,
) -> anyhow::Result<()> {
    let inst = Instance::open(name)?;
    let status = inst.reconcile_status().await?;
    if status == Status::Suspended {
        anyhow::bail!(
            "VM '{name}' is suspended. Resume it with: agv resume {name}"
        );
    }

    // Handle --retry: VM must be broken with non-complete provision state.
    if retry {
        if status != Status::Broken {
            anyhow::bail!(
                "--retry requires VM '{name}' to be in broken state (currently {status})"
            );
        }
        let state = inst.read_provision_state().await;
        if state.is_complete() {
            anyhow::bail!(
                "VM '{name}' has no failed provisioning to retry — provisioning already completed"
            );
        }
    } else {
        // Normal start: VM must be stopped, OR broken with QEMU still running
        // (in which case we tell the user to use --retry).
        if status == Status::Broken {
            anyhow::bail!(
                "VM '{name}' is broken. Use 'agv start --retry {name}' to resume \
                 provisioning, or 'agv destroy {name}' to start over."
            );
        }
        anyhow::ensure!(
            status == Status::Stopped,
            Error::VmBadState {
                name: name.to_string(),
                status: status.to_string(),
                expected: "stopped".to_string(),
            }
        );
    }

    let config = crate::config::load_resolved(&inst.config_path())?;

    let spinner = status_spinner(verbose, quiet);

    // Start QEMU only if it's not already running (a broken VM may still
    // have QEMU alive — the user wants to retry, not restart from scratch).
    let qemu_already_running = retry && inst.is_process_alive().await;
    if qemu_already_running {
        step_done(&spinner, "QEMU already running — retrying provisioning");
    } else {
        spinner.set_message(format!(
            "Starting QEMU ({} RAM, {} vCPUs)...",
            config.memory, config.cpus
        ));
        qemu::start(&inst, &config.memory, config.cpus).await?;
        step_done(
            &spinner,
            &format!("Started QEMU ({} RAM, {} vCPUs)", config.memory, config.cpus),
        );
    }
    inst.write_status(Status::Running).await?;

    // Run first boot (resumes from saved state if any) or wait for SSH.
    let first_boot_result = if inst.is_provisioned() {
        wait_for_ssh(&inst, &config.user, &spinner).await.map(|()| {
            step_done(&spinner, "SSH is ready");
        })
    } else {
        run_first_boot(&inst, &config, interactive_mode, verbose, quiet, &spinner).await
    };

    if let Err(e) = first_boot_result {
        // Mark broken and persist the error. Leave QEMU running so the
        // user can SSH in to debug (assuming SSH came up at all).
        mark_broken_with_error(&inst, &e).await;
        return Err(e);
    }

    // Apply forwards only after SSH is up — the supervisors would otherwise
    // burn through retry cycles waiting for sshd, and the success message
    // would print before any forward could possibly work.
    apply_and_report_forwards(&inst, &config, &spinner).await;

    update_ssh_config(&inst, &config.user).await;

    spinner.finish_with_message(format!("  ✓ VM '{name}' is running"));
    Ok(())
}

/// Print detailed information about a VM instance.
pub async fn inspect(name: &str) -> anyhow::Result<()> {
    let inst = Instance::open(name)?;
    let status = inst.reconcile_status().await?;
    let config = crate::config::load_resolved(&inst.config_path())?;
    let provision_state = inst.read_provision_state().await;

    // Header: name and status. For broken VMs, append a substate.
    if status == Status::Broken {
        println!("{name}  {status} ({})", broken_substate(&provision_state));
    } else {
        println!("{name}  {status}");
    }

    println!();
    let w = 11; // label column width

    // Hardware summary.
    println!(
        "  {:<w$}  {}  {} vCPUs  {} disk",
        "Hardware", config.memory, config.cpus, config.disk
    );
    println!("  {:<w$}  {}", "User", config.user);

    // SSH connection info — meaningful when running, or broken-but-SSH-came-up.
    let ssh_might_work = status == Status::Running
        || (status == Status::Broken && provision_state.phase != Phase::SshWait);
    if ssh_might_work {
        let port_raw = tokio::fs::read_to_string(inst.ssh_port_path())
            .await
            .unwrap_or_default();
        let port = port_raw.trim();
        if !port.is_empty() {
            println!("  {:<w$}  localhost:{port}", "SSH port");
        }
    }

    // Auto-allocated forwards (e.g. from a `gui-xfce` mixin exposing RDP).
    // Only meaningful on a running VM — when the VM is stopped the port
    // files are cleaned up, so the BTreeMap lookup is fine either way.
    if status == Status::Running && !config.auto_forwards.is_empty() {
        for name in config.auto_forwards.keys() {
            let raw = tokio::fs::read_to_string(inst.auto_forward_port_path(name))
                .await
                .unwrap_or_default();
            let port = raw.trim();
            if !port.is_empty() {
                println!("  {:<w$}  localhost:{port}", format!("{name} port"));
            }
        }
    }

    let provisioned = if inst.is_provisioned() { "yes" } else { "no" };
    println!("  {:<w$}  {provisioned}", "Provisioned");
    println!("  {:<w$}  {}", "Data dir", inst.dir.display());

    // Show error log for broken VMs.
    if status == Status::Broken {
        let error_log = inst.error_log_path();
        if error_log.exists() {
            let content = tokio::fs::read_to_string(&error_log)
                .await
                .unwrap_or_default();
            println!();
            println!("  Error");
            for line in content.trim().lines() {
                println!("    {line}");
            }
        }
        // Hint how to recover.
        println!();
        if provision_state.phase == Phase::SshWait {
            println!("  Hint: SSH never came up. Try 'agv destroy {name}' and create again.");
        } else if !provision_state.is_complete() {
            println!("  Hint: 'agv start --retry {name}' to resume from the failed step,");
            println!("        or 'agv destroy {name}' to start over.");
        }
    }

    Ok(())
}

/// Build a short description of where a broken VM failed.
#[must_use]
pub fn broken_substate(state: &ProvisionState) -> String {
    match state.phase {
        Phase::SshWait => "ssh timeout".to_string(),
        Phase::Files => format!("files step {}/{}", state.index + 1, state.total),
        Phase::Setup => format!("setup step {}/{}", state.index + 1, state.total),
        Phase::Provision => format!("provision step {}/{}", state.index + 1, state.total),
        Phase::Complete => "post-provisioning failure".to_string(),
    }
}

/// Stop a running VM. If `force` is true, kill the process immediately.
pub async fn stop(name: &str, force: bool) -> anyhow::Result<()> {
    let inst = Instance::open(name)?;
    let status = inst.reconcile_status().await?;
    anyhow::ensure!(
        status == Status::Running,
        Error::VmBadState {
            name: name.to_string(),
            status: status.to_string(),
            expected: "running".to_string(),
        }
    );
    // Tear down forward supervisors before QEMU exits, so they don't spend
    // a few seconds retrying against a dying SSH server.
    forwarding::stop_all_for_instance(&inst).await;
    if force {
        qemu::force_stop(&inst).await?;
    } else {
        qemu::stop(&inst).await?;
    }
    inst.write_status(Status::Stopped).await?;
    let _ = ssh_config::remove_entry(name).await;
    Ok(())
}

/// Suspend a running VM by saving its state to a snapshot, then exit QEMU.
///
/// The VM can be brought back with `resume`. The snapshot is stored inside
/// the qcow2 disk, so no extra files are created. Note: the disk file grows
/// by roughly the VM's RAM usage.
pub async fn suspend(name: &str) -> anyhow::Result<()> {
    let inst = Instance::open(name)?;
    let status = inst.reconcile_status().await?;
    anyhow::ensure!(
        status == Status::Running,
        Error::VmBadState {
            name: name.to_string(),
            status: status.to_string(),
            expected: "running".to_string(),
        }
    );
    forwarding::stop_all_for_instance(&inst).await;
    qemu::suspend(&inst).await?;
    inst.write_status(Status::Suspended).await?;
    let _ = ssh_config::remove_entry(name).await;
    Ok(())
}

/// Resume a suspended VM by starting QEMU with the saved snapshot.
pub async fn resume(name: &str, verbose: bool, quiet: bool) -> anyhow::Result<()> {
    let inst = Instance::open(name)?;
    let status = inst.reconcile_status().await?;
    anyhow::ensure!(
        status == Status::Suspended,
        Error::VmBadState {
            name: name.to_string(),
            status: status.to_string(),
            expected: "suspended".to_string(),
        }
    );

    let config = crate::config::load_resolved(&inst.config_path())?;

    let spinner = status_spinner(verbose, quiet);
    spinner.set_message(format!(
        "Resuming VM ({} RAM, {} vCPUs)...",
        config.memory, config.cpus
    ));

    qemu::start_with_loadvm(&inst, &config.memory, config.cpus, Some("agv-suspend")).await?;
    inst.write_status(Status::Running).await?;
    step_done(&spinner, "Resumed VM");

    wait_for_ssh(&inst, &config.user, &spinner).await?;
    step_done(&spinner, "SSH is ready");

    apply_and_report_forwards(&inst, &config, &spinner).await;

    update_ssh_config(&inst, &config.user).await;

    spinner.finish_with_message(format!("  ✓ VM '{name}' is running"));
    Ok(())
}

/// Destroy a VM — stop it if needed, then delete all its state.
///
/// Refuses to destroy a running VM unless `force` is set, to prevent
/// accidental data loss.
/// Rename a VM. Requires the VM to be stopped, suspended, or broken
/// (renaming a running VM would move files out from under QEMU).
///
/// Moves the instance directory, updates the managed SSH config, and
/// returns whether the guest hostname should be updated manually.
pub async fn rename(old: &str, new: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        old != new,
        "old and new names are identical: '{old}'"
    );
    anyhow::ensure!(
        !new.is_empty(),
        "new name cannot be empty"
    );
    anyhow::ensure!(
        !new.contains('/') && !new.contains('\\') && !new.contains('\0'),
        "new name contains invalid characters: '{new}'"
    );

    let inst = Instance::open(old)?;
    let status = inst.reconcile_status().await?;
    anyhow::ensure!(
        matches!(status, Status::Stopped | Status::Suspended | Status::Broken),
        Error::VmBadState {
            name: old.to_string(),
            status: status.to_string(),
            expected: "stopped, suspended, or broken".to_string(),
        }
    );

    let new_dir = dirs::instance_dir(new)?;
    if new_dir.exists() {
        return Err(Error::VmAlreadyExists {
            name: new.to_string(),
        }
        .into());
    }

    // Remove the old SSH config entry (usually already gone if stopped).
    let _ = ssh_config::remove_entry(old).await;

    // Move the directory.
    tokio::fs::rename(&inst.dir, &new_dir)
        .await
        .with_context(|| {
            format!(
                "failed to rename instance directory {} → {}",
                inst.dir.display(),
                new_dir.display()
            )
        })?;

    info!(old, new, "VM renamed");
    Ok(())
}

pub async fn destroy(name: &str, force: bool) -> anyhow::Result<()> {
    let inst = Instance::open(name)?;
    let status = inst.reconcile_status().await?;

    if status == Status::Running {
        anyhow::ensure!(
            force,
            "VM '{name}' is running — stop it first, or pass --force to destroy it anyway"
        );
        forwarding::stop_all_for_instance(&inst).await;
        let _ = qemu::force_stop(&inst).await;
    } else {
        // Even on a stopped/broken VM, sweep any stale supervisors that
        // a previous run might have left in forwards.toml.
        forwarding::stop_all_for_instance(&inst).await;
    }

    let _ = ssh_config::remove_entry(name).await;

    tokio::fs::remove_dir_all(&inst.dir)
        .await
        .with_context(|| format!("failed to remove instance directory for VM '{name}'"))?;
    Ok(())
}

/// List all known VM instances.
pub async fn list() -> anyhow::Result<Vec<Instance>> {
    let dir = dirs::instances_dir()?;
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries = tokio::fs::read_dir(&dir)
        .await
        .context("failed to read instances directory")?;
    let mut instances = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            instances.push(Instance {
                name,
                dir: entry.path(),
            });
        }
    }
    instances.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(instances)
}

