//! VM lifecycle management — create, start, stop, destroy.
//!
//! This module orchestrates the high-level VM operations, delegating to
//! submodules for QEMU process management, cloud-init, and instance state.

pub mod cloud_init;
pub mod instance;
pub mod qemu;

use anyhow::Context as _;

use crate::config::Config;
use instance::Instance;

/// Create a new VM from the given configuration.
pub async fn create(_name: &str, _config: &Config, _start_after: bool) -> anyhow::Result<()> {
    todo!("orchestrate full VM creation: image, disk, keys, cloud-init, boot, provision")
}

/// Start an existing stopped VM.
pub async fn start(name: &str) -> anyhow::Result<()> {
    let inst = Instance::open(name).await?;
    let status = inst.reconcile_status().await?;
    anyhow::ensure!(
        status == instance::Status::Stopped,
        crate::error::Error::VmBadState {
            name: name.to_string(),
            status: status.to_string(),
            expected: "stopped".to_string(),
        }
    );

    let config = crate::config::load(&inst.config_path())?;
    let memory = config
        .vm
        .as_ref()
        .and_then(|vm| vm.memory.as_deref())
        .unwrap_or("2G");
    let cpus = config.vm.as_ref().and_then(|vm| vm.cpus).unwrap_or(2);

    qemu::start(&inst, memory, cpus).await?;
    inst.write_status(instance::Status::Running).await?;
    Ok(())
}

/// Stop a running VM. If `force` is true, kill the process immediately.
pub async fn stop(name: &str, force: bool) -> anyhow::Result<()> {
    let inst = Instance::open(name).await?;
    let status = inst.reconcile_status().await?;
    anyhow::ensure!(
        status == instance::Status::Running,
        crate::error::Error::VmBadState {
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
    inst.write_status(instance::Status::Stopped).await?;
    Ok(())
}

/// Destroy a VM — remove all its state regardless of current status.
pub async fn destroy(name: &str) -> anyhow::Result<()> {
    let inst = Instance::open(name).await?;
    // If running, stop first.
    if inst.reconcile_status().await? == instance::Status::Running {
        let _ = qemu::force_stop(&inst).await;
    }
    tokio::fs::remove_dir_all(&inst.dir)
        .await
        .with_context(|| format!("failed to remove instance directory for VM '{name}'"))?;
    Ok(())
}

/// List all known VM instances.
pub async fn list() -> anyhow::Result<Vec<Instance>> {
    let dir = crate::dirs::instances_dir()?;
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
