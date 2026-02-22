//! VM lifecycle management — create, start, stop, destroy.
//!
//! This module orchestrates the high-level VM operations, delegating to
//! submodules for QEMU process management, cloud-init, and instance state.

pub mod cloud_init;
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
use crate::{dirs, image, ssh};
use instance::{Instance, Status};

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

/// Print a completed-step line above the spinner, keeping previous output visible.
fn step_done(spinner: &ProgressBar, msg: &str) {
    spinner.println(format!("  ✓ {msg}"));
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
pub async fn create(
    name: &str,
    config: &ResolvedConfig,
    start_after: bool,
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
    if let Err(e) = create_inner(&inst, name, config, start_after, verbose, quiet).await {
        // Mark as broken so users can inspect / destroy.
        let _ = inst.write_status(Status::Broken).await;
        let _ = tokio::fs::write(
            inst.error_log_path(),
            format!("{e:#}"),
        )
        .await;
        return Err(e);
    }

    Ok(())
}

/// Inner creation logic — does all real work, uses `?` for early return.
async fn create_inner(
    inst: &Instance,
    name: &str,
    config: &ResolvedConfig,
    start_after: bool,
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

    // Gather files for cloud-init injection.
    let files: Vec<(String, String)> = config
        .files
        .iter()
        .map(|f| (f.source.clone(), f.dest.clone()))
        .collect();

    // Generate cloud-init seed ISO.
    let file_count = files.len();
    if file_count > 0 {
        spinner.set_message(format!("Generating cloud-init seed ({file_count} files)..."));
    } else {
        spinner.set_message("Generating cloud-init seed...");
    }
    info!("generating cloud-init seed ISO");
    cloud_init::generate_seed(&inst.seed_path(), &pub_key, name, &config.user, &files).await?;
    if file_count > 0 {
        step_done(&spinner, &format!("Generated cloud-init seed ({file_count} files)"));
    } else {
        step_done(&spinner, "Generated cloud-init seed");
    }

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
    run_first_boot(inst, config, verbose, quiet, &spinner).await?;

    spinner.finish_with_message(format!("  ✓ VM '{name}' is running"));
    info!(name, "VM created and running");
    Ok(())
}

/// Execute setup steps as root via `sudo`. First failure aborts remaining steps.
///
/// SSH connects as the configured user (the only user with an authorized key)
/// and wraps each command with `sudo` to gain root privileges.
/// Output is captured to `provision.log`; with `verbose`, also written to stderr.
async fn run_setup(
    instance: &Instance,
    user: &str,
    steps: &[ProvisionStep],
    verbose: bool,
    spinner: &ProgressBar,
) -> anyhow::Result<()> {
    let total = steps.len();
    for (i, step) in steps.iter().enumerate() {
        let num = i + 1;
        let label = step_label(step);
        spinner.set_message(format!("Running setup ({num}/{total}): {label}..."));

        if let Some(ref script) = step.run {
            info!(step = num, "running inline setup script (as root)");
            let output = ssh::run_cmd(
                instance,
                user,
                &[format!("sudo bash -c {}", shell_escape(script))],
            )
            .await
            .with_context(|| format!("setup step {num}: inline script failed"))?;
            append_provision_log(instance, &format!("=== setup step {num} ({label}) ===\n{output}")).await?;
            if verbose {
                eprint!("{output}");
            }
        } else if let Some(ref script_path) = step.script {
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
async fn run_provision_steps(
    instance: &Instance,
    user: &str,
    steps: &[ProvisionStep],
    verbose: bool,
    spinner: &ProgressBar,
) -> anyhow::Result<()> {
    let total = steps.len();
    for (i, step) in steps.iter().enumerate() {
        let num = i + 1;
        let label = step_label(step);
        spinner.set_message(format!("Running provision ({num}/{total}): {label}..."));

        if let Some(ref script) = step.run {
            info!(step = num, "running inline provisioning script");
            let output = ssh::run_cmd(
                instance,
                user,
                &[format!("bash -c {}", shell_escape(script))],
            )
            .await
            .with_context(|| format!("provisioning step {num}: inline script failed"))?;
            append_provision_log(instance, &format!("=== provision step {num} ({label}) ===\n{output}")).await?;
            if verbose {
                eprint!("{output}");
            }
        } else if let Some(ref script_path) = step.script {
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

/// Start an existing stopped VM.
///
/// If the VM has never been provisioned, runs the full provisioning flow
/// (wait for SSH, setup steps, provision steps) after starting QEMU.
pub async fn start(name: &str, verbose: bool, quiet: bool) -> anyhow::Result<()> {
    let inst = Instance::open(name).await?;
    let status = inst.reconcile_status().await?;
    anyhow::ensure!(
        status == Status::Stopped,
        Error::VmBadState {
            name: name.to_string(),
            status: status.to_string(),
            expected: "stopped".to_string(),
        }
    );

    let config = crate::config::load_resolved(&inst.config_path())?;

    let spinner = status_spinner(verbose, quiet);
    spinner.set_message(format!(
        "Starting QEMU ({} RAM, {} vCPUs)...",
        config.memory, config.cpus
    ));

    qemu::start(&inst, &config.memory, config.cpus).await?;
    inst.write_status(Status::Running).await?;
    step_done(
        &spinner,
        &format!("Started QEMU ({} RAM, {} vCPUs)", config.memory, config.cpus),
    );

    if !inst.is_provisioned() {
        run_first_boot(&inst, &config, verbose, quiet, &spinner).await?;
    }

    spinner.finish_with_message(format!("  ✓ VM '{name}' is running"));
    Ok(())
}

/// Run the full first-boot provisioning flow: wait for SSH, setup, provision.
///
/// Called by both `create()` (with `--start`) and `start()` (first boot).
async fn run_first_boot(
    inst: &Instance,
    config: &crate::config::ResolvedConfig,
    verbose: bool,
    _quiet: bool,
    spinner: &ProgressBar,
) -> anyhow::Result<()> {
    spinner.set_message("Waiting for SSH...");
    ssh::wait_for_ready(inst, &config.user).await?;
    step_done(spinner, "SSH is ready");

    if !config.setup.is_empty() {
        run_setup(inst, &config.user, &config.setup, verbose, spinner).await?;
    }

    if !config.provision.is_empty() {
        run_provision_steps(inst, &config.user, &config.provision, verbose, spinner).await?;
    }

    inst.mark_provisioned().await?;
    Ok(())
}

/// Print detailed information about a VM instance.
pub async fn inspect(name: &str) -> anyhow::Result<()> {
    let inst = Instance::open(name).await?;
    let status = inst.reconcile_status().await?;
    let config = crate::config::load_resolved(&inst.config_path())?;

    // Header: name and status.
    println!("{name}  {status}");

    println!();
    let w = 11; // label column width
    println!("  {:<w$}  {}", "Memory", config.memory);
    println!("  {:<w$}  {}", "CPUs", config.cpus);
    println!("  {:<w$}  {}", "Disk", config.disk);
    println!("  {:<w$}  {}", "User", config.user);

    // Show template or base image origin.
    if let Some(ref tname) = config.template_name {
        println!("  {:<w$}  {tname}", "Template");
    } else {
        let image_label = config
            .base_url
            .rsplit('/')
            .next()
            .unwrap_or(&config.base_url);
        println!("  {:<w$}  {image_label}", "Base image");
    }

    // SSH connection command — only meaningful when running.
    if status == Status::Running {
        let port_raw = tokio::fs::read_to_string(inst.ssh_port_path())
            .await
            .unwrap_or_default();
        let port = port_raw.trim();
        if !port.is_empty() {
            let key = inst.ssh_key_path();
            let key_str = key.display();
            println!(
                "  {:<w$}  ssh -i \"{key_str}\" -p {port} {}@localhost",
                "SSH", config.user
            );
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
    }

    Ok(())
}

/// Stop a running VM. If `force` is true, kill the process immediately.
pub async fn stop(name: &str, force: bool) -> anyhow::Result<()> {
    let inst = Instance::open(name).await?;
    let status = inst.reconcile_status().await?;
    anyhow::ensure!(
        status == Status::Running,
        Error::VmBadState {
            name: name.to_string(),
            status: status.to_string(),
            expected: "running".to_string(),
        }
    );
    if force {
        qemu::force_stop(&inst).await?;
    } else {
        qemu::stop(&inst).await?;
    }
    inst.write_status(Status::Stopped).await?;
    Ok(())
}

/// Destroy a VM — remove all its state regardless of current status.
pub async fn destroy(name: &str) -> anyhow::Result<()> {
    let inst = Instance::open(name).await?;
    // If running, stop first.
    if inst.reconcile_status().await? == Status::Running {
        let _ = qemu::force_stop(&inst).await;
    }
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
    let inst = Instance::open(vm_name).await?;
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
        run_first_boot(&inst, &config, verbose, quiet, &spinner).await?;
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
        spinner.set_message("Waiting for SSH...");
        ssh::wait_for_ready(&inst, &config.user).await?;
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

/// List all available templates.
pub fn list_templates() -> anyhow::Result<Vec<TemplateInfo>> {
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
            templates.push(TemplateInfo {
                name: meta.name,
                source_vm: meta.source_vm,
                memory: meta.memory,
                cpus: meta.cpus,
                disk: meta.disk,
            });
        }
    }

    templates.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(templates)
}

/// Create a new VM as a thin clone of an existing template.
///
/// The clone shares the template's disk via a qcow2 overlay (copy-on-write),
/// gets a fresh SSH keypair, and receives a new cloud-init seed with its own
/// hostname. Provisioning steps are not re-run — the template already contains
/// a fully configured system.
#[allow(clippy::too_many_arguments)]
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
#[allow(clippy::too_many_arguments)]
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
    cloud_init::generate_seed(&inst.seed_path(), &pub_key, vm_name, &meta.user, &[]).await?;
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
    spinner.set_message("Waiting for SSH (cloud-init applying settings)...");
    ssh::wait_for_ready(inst, &meta.user).await?;
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
