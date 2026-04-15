//! VM lifecycle management — create, start, stop, destroy.
//!
//! This module orchestrates the high-level VM operations, delegating to
//! submodules for QEMU process management, cloud-init, and instance state.

pub mod cloud_init;
pub mod forwarding;
pub mod instance;
pub mod qemu;

use std::io::IsTerminal as _;
use std::path::Path;
use std::time::Duration;

use anyhow::Context as _;
use indicatif::ProgressBar;
use tracing::info;

use serde::{Deserialize, Serialize};

use crate::config::{ProvisionStep, ResolvedConfig};
use crate::error::Error;
use crate::interactive::InteractiveState;
use crate::{dirs, image, interactive, ssh, ssh_config};
use instance::{Instance, Phase, ProvisionState, Status};

/// Create an indicatif spinner for status messages.
///
/// Returns a hidden (no-op) bar when `quiet` is set or stderr is not a TTY
/// (and `verbose` is not set). With `verbose`, always shows status.
fn status_spinner(verbose: bool, quiet: bool) -> ProgressBar {
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
    let _ = ssh_config::add_entry(&inst.name, port, user, &inst.ssh_key_path()).await;
}

/// Print a completed-step line above the spinner, keeping previous output visible.
fn step_done(spinner: &ProgressBar, msg: &str) {
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
    if config.forwards.is_empty() {
        // Still clear any stale state left from a previous boot.
        let _ = crate::forward::clear_active(&inst.forwards_path()).await;
        return;
    }
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

/// Mark a VM as broken and persist the error to all the relevant places.
///
/// Used by both `create()` and `start()` when first-boot provisioning fails.
/// Updates: status → broken, `error.log`, `provision_state.error`.
async fn mark_broken_with_error(inst: &Instance, error: &anyhow::Error) {
    let msg = format!("{error:#}");
    let _ = inst.write_status(Status::Broken).await;
    let _ = tokio::fs::write(inst.error_log_path(), &msg).await;
    let mut state = inst.read_provision_state().await;
    state.error = Some(msg);
    let _ = inst.write_provision_state(&state).await;
}

/// Append output text to the provision log file.
async fn append_provision_log(instance: &Instance, text: &str) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt as _;
    let path = instance.provision_log_path();
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
        .with_context(|| format!("failed to open provision log {}", path.display()))?;
    file.write_all(text.as_bytes()).await?;
    Ok(())
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

    // Update managed SSH config so IDEs can connect by VM name.
    update_ssh_config(inst, &config.user).await;

    spinner.finish_with_message(format!("  ✓ VM '{name}' is running"));
    info!(name, "VM created and running");
    Ok(())
}

/// Execute setup steps as root via `sudo`. First failure aborts remaining steps.
///
/// SSH connects as the configured user (the only user with an authorized key)
/// and wraps each command with `sudo` to gain root privileges.
/// Output is captured to `provision.log`; with `verbose`, also written to stderr.
#[expect(
    clippy::too_many_arguments,
    reason = "internal helper threading instance + user + steps + start_index + interactive state + spinner; the parameters are distinct and refactoring would just shuffle them"
)]
async fn run_setup(
    instance: &Instance,
    user: &str,
    steps: &[ProvisionStep],
    start_index: usize,
    interactive_mode: bool,
    int_state: &mut InteractiveState,
    verbose: bool,
    spinner: &ProgressBar,
) -> anyhow::Result<()> {
    let total = steps.len();
    for (i, step) in steps.iter().enumerate().skip(start_index) {
        instance
            .write_provision_state(&ProvisionState {
                phase: Phase::Setup,
                index: i,
                total,
                error: None,
            })
            .await?;

        let num = i + 1;
        let label = step_label(step);
        spinner.set_message(format!("Running setup ({num}/{total}): {label}..."));

        if let Some(ref script) = step.run {
            // Interactive prompt — may edit the inline script.
            let to_run = if interactive_mode && !int_state.all {
                let decision = spinner.suspend(|| {
                    interactive::prompt_step(&format!("setup {num}/{total}"), script)
                })?;
                match decision {
                    interactive::Decision::Run(cmd) => cmd,
                    interactive::Decision::All(cmd) => {
                        int_state.all = true;
                        cmd
                    }
                    interactive::Decision::Skip => continue,
                    interactive::Decision::Quit => return Err(interactive::user_quit_error()),
                }
            } else {
                script.clone()
            };

            info!(step = num, "running inline setup script (as root)");
            let output = ssh::run_cmd(
                instance,
                user,
                &[format!("sudo bash -c {}", shell_escape(&to_run))],
            )
            .await
            .with_context(|| format!("setup step {num}: inline script failed"))?;
            append_provision_log(instance, &format!("=== setup step {num} ({label}) ===\n{output}")).await?;
            if verbose {
                eprint!("{output}");
            }
        } else if let Some(ref script_path) = step.script {
            // Interactive prompt — script files can't be edited inline.
            if interactive_mode && !int_state.all {
                let display = format!("(script file) {script_path}");
                let decision = spinner.suspend(|| {
                    interactive::prompt_step(&format!("setup {num}/{total}"), &display)
                })?;
                match decision {
                    interactive::Decision::Run(_) => {}
                    interactive::Decision::All(_) => int_state.all = true,
                    interactive::Decision::Skip => {
                        step_done(spinner, &format!("Setup ({num}/{total}): {label} (skipped)"));
                        continue;
                    }
                    interactive::Decision::Quit => return Err(interactive::user_quit_error()),
                }
            }

            info!(step = num, path = script_path, "running setup script file (as root)");
            let remote_path = format!("/tmp/agv-setup-{i}.sh");

            ssh::copy_to(instance, user, Path::new(script_path), &remote_path)
                .await
                .with_context(|| {
                    format!("setup step {num}: failed to copy script {script_path}")
                })?;

            let output = ssh::run_cmd(
                instance,
                user,
                &[format!(
                    "sudo bash -c {}",
                    shell_escape(&format!("chmod +x {remote_path} && {remote_path}"))
                )],
            )
            .await
            .with_context(|| {
                format!("setup step {num}: script {script_path} failed")
            })?;
            append_provision_log(instance, &format!("=== setup step {num} ({label}) ===\n{output}")).await?;
            if verbose {
                eprint!("{output}");
            }
        }

        step_done(spinner, &format!("Setup ({num}/{total}): {label}"));
    }

    info!("setup complete");
    Ok(())
}

/// Execute provisioning steps in order. First failure aborts remaining steps.
///
/// Output is captured to `provision.log`; with `verbose`, also written to stderr.
#[expect(
    clippy::too_many_arguments,
    reason = "internal helper threading instance + user + steps + start_index + interactive state + spinner; the parameters are distinct and refactoring would just shuffle them"
)]
async fn run_provision_steps(
    instance: &Instance,
    user: &str,
    steps: &[ProvisionStep],
    start_index: usize,
    interactive_mode: bool,
    int_state: &mut InteractiveState,
    verbose: bool,
    spinner: &ProgressBar,
) -> anyhow::Result<()> {
    let total = steps.len();
    for (i, step) in steps.iter().enumerate().skip(start_index) {
        instance
            .write_provision_state(&ProvisionState {
                phase: Phase::Provision,
                index: i,
                total,
                error: None,
            })
            .await?;

        let num = i + 1;
        let label = step_label(step);
        spinner.set_message(format!("Running provision ({num}/{total}): {label}..."));

        if let Some(ref script) = step.run {
            // Interactive prompt — may edit the inline script.
            let to_run = if interactive_mode && !int_state.all {
                let decision = spinner.suspend(|| {
                    interactive::prompt_step(&format!("provision {num}/{total}"), script)
                })?;
                match decision {
                    interactive::Decision::Run(cmd) => cmd,
                    interactive::Decision::All(cmd) => {
                        int_state.all = true;
                        cmd
                    }
                    interactive::Decision::Skip => continue,
                    interactive::Decision::Quit => return Err(interactive::user_quit_error()),
                }
            } else {
                script.clone()
            };

            info!(step = num, "running inline provisioning script");
            let output = ssh::run_cmd(
                instance,
                user,
                &[format!("bash -c {}", shell_escape(&to_run))],
            )
            .await
            .with_context(|| format!("provisioning step {num}: inline script failed"))?;
            append_provision_log(instance, &format!("=== provision step {num} ({label}) ===\n{output}")).await?;
            if verbose {
                eprint!("{output}");
            }
        } else if let Some(ref script_path) = step.script {
            // Interactive prompt — script files can't be edited inline.
            if interactive_mode && !int_state.all {
                let display = format!("(script file) {script_path}");
                let decision = spinner.suspend(|| {
                    interactive::prompt_step(&format!("provision {num}/{total}"), &display)
                })?;
                match decision {
                    interactive::Decision::Run(_) => {}
                    interactive::Decision::All(_) => int_state.all = true,
                    interactive::Decision::Skip => {
                        step_done(spinner, &format!("Provision ({num}/{total}): {label} (skipped)"));
                        continue;
                    }
                    interactive::Decision::Quit => return Err(interactive::user_quit_error()),
                }
            }

            info!(step = num, path = script_path, "running provisioning script file");
            let remote_path = format!("/tmp/agv-provision-{i}.sh");

            ssh::copy_to(instance, user, Path::new(script_path), &remote_path)
                .await
                .with_context(|| {
                    format!("provisioning step {num}: failed to copy script {script_path}")
                })?;

            let output = ssh::run_cmd(
                instance,
                user,
                &[format!("chmod +x {remote_path} && {remote_path}")],
            )
            .await
            .with_context(|| {
                format!("provisioning step {num}: script {script_path} failed")
            })?;
            append_provision_log(instance, &format!("=== provision step {num} ({label}) ===\n{output}")).await?;
            if verbose {
                eprint!("{output}");
            }
        }

        step_done(spinner, &format!("Provision ({num}/{total}): {label}"));
    }

    info!("provisioning complete");
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

    apply_and_report_forwards(&inst, &config, &spinner).await;

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

    update_ssh_config(&inst, &config.user).await;

    spinner.finish_with_message(format!("  ✓ VM '{name}' is running"));
    Ok(())
}

/// Wait for SSH with a live elapsed-time counter in the spinner message.
async fn wait_for_ssh(inst: &Instance, user: &str, spinner: &ProgressBar) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    spinner.set_message("Waiting for SSH...");
    let result = tokio::select! {
        result = ssh::wait_for_ready(inst, user) => result,
        _ = async {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                spinner.set_message(format!(
                    "Waiting for SSH... ({}s)",
                    start.elapsed().as_secs()
                ));
            }
        } => unreachable!(),
    };
    result
}

/// Copy `[[files]]` entries into the VM via SCP.
///
/// Creates parent directories for each destination, then copies the file.
/// Errors are reported immediately — unlike cloud-init `write_files`, failures
/// are never silent.
async fn copy_files(
    instance: &Instance,
    user: &str,
    files: &[crate::config::FileEntry],
    start_index: usize,
    interactive_mode: bool,
    int_state: &mut InteractiveState,
    spinner: &ProgressBar,
) -> anyhow::Result<()> {
    let total = files.len();
    for (i, file) in files.iter().enumerate().skip(start_index) {
        // Update state to "about to run step i".
        instance
            .write_provision_state(&ProvisionState {
                phase: Phase::Files,
                index: i,
                total,
                error: None,
            })
            .await?;

        let num = i + 1;
        let label = file
            .source
            .rsplit('/')
            .next()
            .unwrap_or(&file.source);
        spinner.set_message(format!("Copying file ({num}/{total}): {label}..."));

        // Interactive prompt: let the user skip a file or quit.
        // Edits don't make sense for file copies.
        if interactive_mode && !int_state.all {
            let display = format!("copy {} → {}", file.source, file.dest);
            let decision = spinner.suspend(|| {
                interactive::prompt_step(&format!("file {num}/{total}"), &display)
            })?;
            match decision {
                interactive::Decision::Run(_) => {}
                interactive::Decision::All(_) => int_state.all = true,
                interactive::Decision::Skip => continue,
                interactive::Decision::Quit => return Err(interactive::user_quit_error()),
            }
        }

        // Ensure the parent directory exists inside the VM.
        let parent_dir = parent_dir_of(&file.dest);
        if !parent_dir.is_empty() && parent_dir != "." && parent_dir != "/" {
            ssh::run_cmd(
                instance,
                user,
                &[format!("mkdir -p {}", shell_escape(parent_dir))],
            )
            .await
            .with_context(|| {
                format!("file {num}: failed to create directory {parent_dir}")
            })?;
        }

        ssh::copy_to(instance, user, Path::new(&file.source), &file.dest)
            .await
            .with_context(|| {
                format!(
                    "file {num}: failed to copy {} → {}",
                    file.source, file.dest
                )
            })?;

        step_done(spinner, &format!("Copied file ({num}/{total}): {label}"));
    }

    Ok(())
}

/// Run the full first-boot provisioning flow: wait for SSH, copy files, setup, provision.
///
/// Called by `create()` (with `--start`) and `start()` (first boot or `--retry`).
/// Reads the existing `provision_state` and resumes from the saved phase/index,
/// so a previously failed run can pick up where it left off without re-running
/// already-completed steps.
///
/// In `interactive_mode`, the user is prompted before each file copy, setup
/// step, and provision step.
async fn run_first_boot(
    inst: &Instance,
    config: &crate::config::ResolvedConfig,
    interactive_mode: bool,
    verbose: bool,
    _quiet: bool,
    spinner: &ProgressBar,
) -> anyhow::Result<()> {
    let state = inst.read_provision_state().await;
    let mut int_state = InteractiveState::new();

    // Always wait for SSH — it's idempotent and we need it for all later phases.
    if state.phase == Phase::SshWait || state.phase == Phase::Files
        || state.phase == Phase::Setup || state.phase == Phase::Provision
    {
        inst.write_provision_state(&ProvisionState {
            phase: Phase::SshWait,
            index: 0,
            total: 0,
            error: None,
        })
        .await?;
        wait_for_ssh(inst, &config.user, spinner).await?;
        step_done(spinner, "SSH is ready");
    }

    // Files phase: skip if we're already past it.
    if state.phase == Phase::SshWait
        || state.phase == Phase::Files
    {
        let files_start = if state.phase == Phase::Files { state.index } else { 0 };
        if !config.files.is_empty() {
            copy_files(
                inst,
                &config.user,
                &config.files,
                files_start,
                interactive_mode,
                &mut int_state,
                spinner,
            )
            .await?;
        }
    }

    // Setup phase.
    if state.phase == Phase::SshWait
        || state.phase == Phase::Files
        || state.phase == Phase::Setup
    {
        let setup_start = if state.phase == Phase::Setup { state.index } else { 0 };
        if !config.setup.is_empty() {
            run_setup(
                inst,
                &config.user,
                &config.setup,
                setup_start,
                interactive_mode,
                &mut int_state,
                verbose,
                spinner,
            )
            .await?;
        }
    }

    // Provision phase.
    let provision_start = if state.phase == Phase::Provision { state.index } else { 0 };
    if !config.provision.is_empty() {
        run_provision_steps(
            inst,
            &config.user,
            &config.provision,
            provision_start,
            interactive_mode,
            &mut int_state,
            verbose,
            spinner,
        )
        .await?;
    }

    inst.mark_provisioned().await?;
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

    apply_and_report_forwards(&inst, &config, &spinner).await;

    wait_for_ssh(&inst, &config.user, &spinner).await?;
    step_done(&spinner, "SSH is ready");

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

/// Produce a short label for a provisioning step.
///
/// If the step has a `source` (i.e. it came from an include module), shows that.
/// Otherwise falls back to the script file path or truncated inline script.
fn step_label(step: &ProvisionStep) -> String {
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

/// Single-quote a string for safe embedding in a shell command.
///
/// Wraps the value in single quotes and escapes any embedded single quotes
/// using the `'\''` idiom (end quote, escaped quote, reopen quote).
fn shell_escape(s: &str) -> String {
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

/// Extract the parent directory from a destination path.
///
/// Returns `"."` when no slash is present, or the portion before the last
/// slash. Used by `copy_files()` to `mkdir -p` before copying.
fn parent_dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or(".", |(dir, _)| dir)
}

// ---------------------------------------------------------------------------
// Template operations
// ---------------------------------------------------------------------------

/// Persistent metadata stored alongside each template disk image.
#[derive(Debug, Serialize, Deserialize)]
struct TemplateMetadata {
    /// Template name (same as filename stem).
    name: String,
    /// Name of the source VM.
    source_vm: String,
    /// Host architecture the template was created on.
    arch: String,
    /// Default memory for VMs cloned from this template.
    memory: String,
    /// Default CPU count for VMs cloned from this template.
    cpus: u32,
    /// Disk size for the backing image.
    disk: String,
    /// Default username for VMs cloned from this template.
    user: String,
}

/// Summary information about an available template.
#[derive(Debug)]
pub struct TemplateInfo {
    pub name: String,
    pub source_vm: String,
    pub memory: String,
    pub cpus: u32,
    pub disk: String,
    /// Names of VM instances currently using this template as a backing image.
    pub dependents: Vec<String>,
}

/// Create a reusable template from an existing VM.
///
/// The VM must be stopped (or `stop_if_running` must be set). If the VM has
/// never been provisioned, a start/provision/stop cycle is run first.
/// Before the disk is converted to a standalone image, the machine-id is
/// cleared via SSH so that every clone boots with a freshly generated ID.
pub async fn create_template(
    vm_name: &str,
    template_name: &str,
    stop_if_running: bool,
    verbose: bool,
    quiet: bool,
) -> anyhow::Result<()> {
    let inst = Instance::open(vm_name)?;
    let mut status = inst.reconcile_status().await?;
    let config = crate::config::load_resolved(&inst.config_path())?;

    let templates_dir = dirs::templates_dir()?;
    tokio::fs::create_dir_all(&templates_dir).await.with_context(|| {
        format!("failed to create templates directory {}", templates_dir.display())
    })?;

    let template_disk = templates_dir.join(format!("{template_name}.qcow2"));
    let template_meta = templates_dir.join(format!("{template_name}.toml"));

    if template_disk.exists() || template_meta.exists() {
        return Err(Error::TemplateAlreadyExists {
            name: template_name.to_string(),
        }
        .into());
    }

    let spinner = status_spinner(verbose, quiet);

    // Handle running VM.
    if status == Status::Running {
        if !stop_if_running {
            anyhow::bail!(
                "VM '{vm_name}' is running — stop it first or pass --stop to do it automatically"
            );
        }
        // Clear machine-id while SSH is accessible, then stop.
        clear_machine_id_via_ssh(&inst, &config.user, &spinner).await?;
        spinner.set_message("Stopping VM...");
        qemu::stop(&inst).await?;
        inst.write_status(Status::Stopped).await?;
        step_done(&spinner, "Stopped VM");
        status = Status::Stopped;
    }

    // Run provisioning if the VM has never been provisioned.
    if !inst.is_provisioned() {
        spinner.set_message(format!(
            "Starting VM for provisioning ({} RAM, {} vCPUs)...",
            config.memory, config.cpus
        ));
        qemu::start(&inst, &config.memory, config.cpus).await?;
        inst.write_status(Status::Running).await?;
        step_done(
            &spinner,
            &format!(
                "Started VM ({} RAM, {} vCPUs)",
                config.memory, config.cpus
            ),
        );
        run_first_boot(&inst, &config, false, verbose, quiet, &spinner).await?;
        status = Status::Running;
    }

    // If the VM is stopped at this point, start it briefly to clear machine-id.
    if status == Status::Stopped {
        spinner.set_message(format!(
            "Starting VM to clear machine-id ({} RAM, {} vCPUs)...",
            config.memory, config.cpus
        ));
        qemu::start(&inst, &config.memory, config.cpus).await?;
        inst.write_status(Status::Running).await?;
        step_done(
            &spinner,
            &format!(
                "Started VM ({} RAM, {} vCPUs)",
                config.memory, config.cpus
            ),
        );
        wait_for_ssh(&inst, &config.user, &spinner).await?;
        step_done(&spinner, "SSH is ready");
    }

    // VM is now running — clear machine-id.
    clear_machine_id_via_ssh(&inst, &config.user, &spinner).await?;

    // Stop the VM.
    spinner.set_message("Stopping VM...");
    qemu::stop(&inst).await?;
    inst.write_status(Status::Stopped).await?;
    step_done(&spinner, "Stopped VM");

    // Flatten overlay + backing chain into a standalone template disk.
    spinner.set_message(format!("Converting disk to template '{template_name}'..."));
    info!(template = template_name, "converting disk to template");
    image::convert_to_template(&inst.disk_path(), &template_disk)
        .await
        .with_context(|| format!("failed to create template disk for '{template_name}'"))?;
    step_done(&spinner, &format!("Created template disk '{template_name}'"));

    // Write template metadata.
    let meta = TemplateMetadata {
        name: template_name.to_string(),
        source_vm: vm_name.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        memory: config.memory.clone(),
        cpus: config.cpus,
        disk: config.disk.clone(),
        user: config.user.clone(),
    };
    let meta_toml =
        toml::to_string_pretty(&meta).context("failed to serialize template metadata")?;
    tokio::fs::write(&template_meta, meta_toml)
        .await
        .with_context(|| {
            format!(
                "failed to write template metadata {}",
                template_meta.display()
            )
        })?;

    spinner.finish_with_message(format!("  ✓ Template '{template_name}' created"));
    info!(template = template_name, vm = vm_name, "template created");
    Ok(())
}

/// List all available templates, including which VMs depend on each.
pub async fn list_templates() -> anyhow::Result<Vec<TemplateInfo>> {
    let templates_dir = dirs::templates_dir()?;
    if !templates_dir.exists() {
        return Ok(Vec::new());
    }

    let mut templates = Vec::new();
    let entries = std::fs::read_dir(&templates_dir)
        .with_context(|| format!("failed to read templates directory {}", templates_dir.display()))?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "toml") {
            let contents = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read template metadata {}", path.display()))?;
            let meta: TemplateMetadata = toml::from_str(&contents)
                .with_context(|| format!("failed to parse template metadata {}", path.display()))?;
            let dependents = find_template_dependents(&meta.name).await?;
            templates.push(TemplateInfo {
                name: meta.name,
                source_vm: meta.source_vm,
                memory: meta.memory,
                cpus: meta.cpus,
                disk: meta.disk,
                dependents,
            });
        }
    }

    templates.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(templates)
}

/// Delete a template by name.
///
/// Fails with [`Error::TemplateNotFound`] if the template does not exist, and
/// with [`Error::TemplateHasDependents`] if any VM instance was created from
/// the template (deleting it would break their overlay disks).
pub async fn remove_template(name: &str) -> anyhow::Result<()> {
    let templates_dir = dirs::templates_dir()?;
    let disk_path = templates_dir.join(format!("{name}.qcow2"));
    let meta_path = templates_dir.join(format!("{name}.toml"));

    if !disk_path.exists() {
        return Err(Error::TemplateNotFound {
            name: name.to_string(),
        }
        .into());
    }

    // Find any instances that were cloned from this template.
    let dependents = find_template_dependents(name).await?;
    if !dependents.is_empty() {
        return Err(Error::TemplateHasDependents {
            name: name.to_string(),
            dependents: dependents.join(", "),
        }
        .into());
    }

    tokio::fs::remove_file(&disk_path)
        .await
        .with_context(|| format!("failed to delete template disk '{name}'"))?;
    // Best-effort: metadata file may not exist for hand-created templates.
    let _ = tokio::fs::remove_file(&meta_path).await;

    Ok(())
}

/// Return the names of all VM instances whose config references the given template.
async fn find_template_dependents(template_name: &str) -> anyhow::Result<Vec<String>> {
    let instances_dir = dirs::instances_dir()?;
    if !instances_dir.exists() {
        return Ok(Vec::new());
    }

    let mut dependents = Vec::new();
    let mut entries = tokio::fs::read_dir(&instances_dir)
        .await
        .with_context(|| format!("failed to read instances directory {}", instances_dir.display()))?;

    while let Some(entry) = entries.next_entry().await? {
        let config_path = entry.path().join("config.toml");
        if !config_path.exists() {
            continue;
        }
        let Ok(contents) = tokio::fs::read_to_string(&config_path).await else {
            continue;
        };
        let Ok(config) = toml::from_str::<crate::config::ResolvedConfig>(&contents) else {
            continue;
        };
        if config.template_name.as_deref() == Some(template_name) {
            let vm_name = entry.file_name().to_string_lossy().into_owned();
            dependents.push(vm_name);
        }
    }

    dependents.sort();
    Ok(dependents)
}

/// Create a new VM as a thin clone of an existing template.
///
/// The clone shares the template's disk via a qcow2 overlay (copy-on-write),
/// gets a fresh SSH keypair, and receives a new cloud-init seed with its own
/// hostname. Provisioning steps are not re-run — the template already contains
/// a fully configured system.
#[expect(
    clippy::too_many_arguments,
    reason = "internal helper threading instance + user + steps + start_index + interactive state + spinner; the parameters are distinct and refactoring would just shuffle them"
)]
pub async fn create_from_template(
    template_name: &str,
    vm_name: &str,
    memory: Option<&str>,
    cpus: Option<u32>,
    disk: Option<&str>,
    start_after: bool,
    verbose: bool,
    quiet: bool,
) -> anyhow::Result<()> {
    let templates_dir = dirs::templates_dir()?;
    let template_disk = templates_dir.join(format!("{template_name}.qcow2"));
    let template_meta_path = templates_dir.join(format!("{template_name}.toml"));

    if !template_disk.exists() {
        return Err(Error::TemplateNotFound {
            name: template_name.to_string(),
        }
        .into());
    }

    // Load template metadata.
    let meta_contents = std::fs::read_to_string(&template_meta_path)
        .with_context(|| format!("failed to read metadata for template '{template_name}'"))?;
    let meta: TemplateMetadata = toml::from_str(&meta_contents)
        .with_context(|| format!("failed to parse metadata for template '{template_name}'"))?;

    // Guard: VM must not already exist.
    let inst_dir = dirs::instance_dir(vm_name)?;
    if inst_dir.exists() {
        return Err(Error::VmAlreadyExists {
            name: vm_name.to_string(),
        }
        .into());
    }

    // Resolve final resource settings (CLI overrides win over template defaults).
    let final_memory = memory.unwrap_or(&meta.memory).to_string();
    let final_cpus = cpus.unwrap_or(meta.cpus);
    let final_disk = disk.unwrap_or(&meta.disk).to_string();

    tokio::fs::create_dir_all(&inst_dir)
        .await
        .with_context(|| {
            format!("failed to create instance directory for VM '{vm_name}'")
        })?;

    let inst = Instance {
        name: vm_name.to_string(),
        dir: inst_dir,
    };

    inst.write_status(Status::Creating).await?;

    if let Err(e) = create_from_template_inner(
        &inst,
        vm_name,
        &template_disk,
        template_name,
        &meta,
        &final_memory,
        final_cpus,
        &final_disk,
        start_after,
        verbose,
        quiet,
    )
    .await
    {
        let _ = inst.write_status(Status::Broken).await;
        let _ = tokio::fs::write(inst.error_log_path(), format!("{e:#}")).await;
        return Err(e);
    }

    Ok(())
}

/// Inner logic for creating a VM from a template.
#[expect(
    clippy::too_many_arguments,
    reason = "internal helper threading instance + user + steps + start_index + interactive state + spinner; the parameters are distinct and refactoring would just shuffle them"
)]
async fn create_from_template_inner(
    inst: &Instance,
    vm_name: &str,
    template_disk: &std::path::Path,
    template_name: &str,
    meta: &TemplateMetadata,
    memory: &str,
    cpus: u32,
    disk: &str,
    start_after: bool,
    verbose: bool,
    quiet: bool,
) -> anyhow::Result<()> {
    let spinner = status_spinner(verbose, quiet);

    // Create qcow2 overlay backed by the template disk.
    spinner.set_message(format!(
        "Creating {disk} overlay on template '{template_name}'..."
    ));
    image::create_overlay(template_disk, &inst.disk_path(), disk).await?;
    step_done(
        &spinner,
        &format!("Created {disk} overlay on template '{template_name}'"),
    );

    // Generate a fresh SSH keypair for this clone.
    spinner.set_message("Generating SSH keypair...");
    let pub_key = ssh::generate_keypair(inst).await?;
    step_done(&spinner, "Generated SSH keypair");

    // Generate cloud-init seed (new hostname + SSH key; no extra files for clones).
    spinner.set_message("Generating cloud-init seed...");
    cloud_init::generate_seed(&inst.seed_path(), &pub_key, vm_name, &meta.user).await?;
    step_done(&spinner, "Generated cloud-init seed");

    // Save a resolved config for this clone so `start` and `inspect` work.
    let clone_config = ResolvedConfig {
        base_url: String::new(),
        base_checksum: String::new(),
        skip_checksum: true,
        memory: memory.to_string(),
        cpus,
        disk: disk.to_string(),
        user: meta.user.clone(),
        files: vec![],
        setup: vec![],
        provision: vec![],
        forwards: vec![],
        template_name: Some(template_name.to_string()),
    };
    crate::config::save(&clone_config, &inst.config_path()).await?;

    // Mark as provisioned — no setup/provision steps to run for template clones.
    inst.mark_provisioned().await?;

    if !start_after {
        inst.write_status(Status::Stopped).await?;
        spinner.finish_with_message(format!("  ✓ VM '{vm_name}' created from template (stopped)"));
        info!(vm = vm_name, template = template_name, "VM created from template (stopped)");
        return Ok(());
    }

    // Start QEMU.
    spinner.set_message(format!(
        "Starting QEMU ({memory} RAM, {cpus} vCPUs)..."
    ));
    qemu::start(inst, memory, cpus).await?;
    inst.write_status(Status::Running).await?;
    step_done(
        &spinner,
        &format!("Started QEMU ({memory} RAM, {cpus} vCPUs)"),
    );

    // Wait for SSH (cloud-init will run to apply hostname + SSH key).
    wait_for_ssh(inst, &meta.user, &spinner).await?;
    step_done(&spinner, "SSH is ready");

    spinner.finish_with_message(format!("  ✓ VM '{vm_name}' is running"));
    info!(vm = vm_name, template = template_name, "VM created from template and running");
    Ok(())
}

/// SSH into the running VM and truncate `/etc/machine-id`.
///
/// This ensures that every clone of a template boots with a freshly
/// generated machine-id rather than inheriting the source VM's identity.
async fn clear_machine_id_via_ssh(
    inst: &Instance,
    user: &str,
    spinner: &ProgressBar,
) -> anyhow::Result<()> {
    spinner.set_message("Clearing machine-id...");
    ssh::run_cmd(
        inst,
        user,
        &["sudo truncate -s 0 /etc/machine-id".to_string()],
    )
    .await
    .context("failed to clear machine-id on VM")?;
    step_done(spinner, "Cleared machine-id");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_dir_of_absolute_path() {
        assert_eq!(parent_dir_of("/home/agent/.ssh/id_ed25519"), "/home/agent/.ssh");
    }

    #[test]
    fn parent_dir_of_root_file() {
        assert_eq!(parent_dir_of("/file.txt"), "");
    }

    #[test]
    fn parent_dir_of_home_file() {
        assert_eq!(parent_dir_of("/home/agent/file.txt"), "/home/agent");
    }

    #[test]
    fn parent_dir_of_no_slash() {
        assert_eq!(parent_dir_of("file.txt"), ".");
    }

    #[test]
    fn parent_dir_of_nested() {
        assert_eq!(parent_dir_of("/a/b/c/d"), "/a/b/c");
    }
}
