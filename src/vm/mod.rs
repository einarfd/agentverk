//! VM lifecycle management — create, start, stop, destroy.
//!
//! This module orchestrates the high-level VM operations, delegating to
//! submodules for QEMU process management, cloud-init, and instance state.

pub mod cloud_init;
pub mod instance;
pub mod qemu;

use std::path::Path;

use anyhow::Context as _;
use tracing::{info, warn};

use crate::config::{ProvisionStep, ResolvedConfig};
use crate::error::Error;
use crate::{dirs, image, ssh};
use instance::{Instance, Status};

/// Create a new VM from the given resolved configuration.
///
/// This is the top-level entry point with error recovery: if creation fails
/// after the instance directory has been created, the VM is marked as broken
/// and the error is logged to `error.log`.
pub async fn create(name: &str, config: &ResolvedConfig, start_after: bool) -> anyhow::Result<()> {
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
    if let Err(e) = create_inner(&inst, name, config, start_after).await {
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
) -> anyhow::Result<()> {
    // Save resolved config to instance dir so restarts / inspect can reload it.
    crate::config::save(config, &inst.config_path()).await?;

    // Cache base image (potentially downloads 500+ MB, idempotent).
    info!(url = %config.base_url, "caching base image");
    let checksum = if config.skip_checksum {
        None
    } else {
        Some(config.base_checksum.as_str())
    };
    let base_image = image::ensure_cached(&config.base_url, checksum).await?;

    // Create qcow2 overlay disk.
    info!(size = %config.disk, "creating overlay disk");
    image::create_overlay(&base_image, &inst.disk_path(), &config.disk).await?;

    // Generate SSH keypair.
    let pub_key = ssh::generate_keypair(inst).await?;

    // Gather files for cloud-init injection.
    let files: Vec<(String, String)> = config
        .files
        .iter()
        .map(|f| (f.source.clone(), f.dest.clone()))
        .collect();

    // Generate cloud-init seed ISO.
    info!("generating cloud-init seed ISO");
    cloud_init::generate_seed(&inst.seed_path(), &pub_key, name, &config.user, &files).await?;

    // If not starting, we're done — write stopped status.
    if !start_after {
        inst.write_status(Status::Stopped).await?;
        if !config.provision.is_empty() {
            warn!(
                "provisioning steps defined but --start not specified — \
                 run `agv start {name}` then `agv provision {name}` to provision"
            );
        }
        info!(name, "VM created (stopped)");
        return Ok(());
    }

    // Start QEMU.
    info!(name, memory = %config.memory, cpus = config.cpus, "starting QEMU");
    qemu::start(inst, &config.memory, config.cpus).await?;
    inst.write_status(Status::Running).await?;

    // Wait for SSH to become ready.
    ssh::wait_for_ready(inst, &config.user).await?;

    // Run provisioning steps (if any).
    if !config.provision.is_empty() {
        run_provisioning(inst, &config.user, &config.provision).await?;
    }

    info!(name, "VM created and running");
    Ok(())
}

/// Execute provisioning steps in order. First failure aborts remaining steps.
async fn run_provisioning(
    instance: &Instance,
    user: &str,
    steps: &[ProvisionStep],
) -> anyhow::Result<()> {
    for (i, step) in steps.iter().enumerate() {
        if let Some(ref script) = step.run {
            info!(step = i + 1, "running inline provisioning script");
            ssh::session(
                instance,
                user,
                &["bash".to_string(), "-c".to_string(), script.clone()],
            )
            .await
            .with_context(|| format!("provisioning step {}: inline script failed", i + 1))?;
        } else if let Some(ref script_path) = step.script {
            info!(step = i + 1, path = script_path, "running provisioning script file");
            let remote_path = format!("/tmp/agv-provision-{i}.sh");

            // Copy the script file to the VM.
            ssh::copy_to(instance, user, Path::new(script_path), &remote_path)
                .await
                .with_context(|| {
                    format!(
                        "provisioning step {}: failed to copy script {script_path}",
                        i + 1
                    )
                })?;

            // Make executable and run.
            ssh::session(
                instance,
                user,
                &[
                    "bash".to_string(),
                    "-c".to_string(),
                    format!("chmod +x {remote_path} && {remote_path}"),
                ],
            )
            .await
            .with_context(|| {
                format!(
                    "provisioning step {}: script {script_path} failed",
                    i + 1
                )
            })?;
        }
    }

    info!("provisioning complete");
    Ok(())
}

/// Start an existing stopped VM.
pub async fn start(name: &str) -> anyhow::Result<()> {
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

    qemu::start(&inst, &config.memory, config.cpus).await?;
    inst.write_status(Status::Running).await?;
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
