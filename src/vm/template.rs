//! Template CRUD — create, list, delete templates, and clone VMs from them.
//!
//! A template is a standalone qcow2 disk under `<data_dir>/templates/` paired
//! with a `.toml` metadata file that records the source VM, architecture, and
//! default resource settings. Cloning a template stamps out a qcow2 overlay
//! that shares the backing disk via copy-on-write; provisioning is skipped
//! because the template already contains a fully configured system.

use std::path::Path;

use anyhow::{bail, Context as _};
use indicatif::ProgressBar;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::config::ResolvedConfig;
use crate::error::Error;
use crate::{dirs, image, ssh};

use super::cloud_init;
use super::instance::{Instance, Status};
use super::qemu;
use super::provision::{run_first_boot, wait_for_ssh};
use super::{status_spinner, step_done};

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
    /// OS family inherited from the source VM. Falls back to `"debian"`
    /// when missing so templates created before this field existed (v0.1.0)
    /// still load — every such template was Debian-family in practice.
    #[serde(default = "default_template_os_family")]
    os_family: String,
}

fn default_template_os_family() -> String {
    "debian".to_string()
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
            bail!(
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
        os_family: config.os_family.clone(),
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
    template_disk: &Path,
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
        os_family: meta.os_family.clone(),
        files: vec![],
        setup: vec![],
        provision: vec![],
        forwards: vec![],
        auto_forwards: std::collections::BTreeMap::new(),
        template_name: Some(template_name.to_string()),
        mixins_applied: vec![],
        mixin_notes: vec![],
        config_notes: vec![],
        mixin_manual_steps: vec![],
        config_manual_steps: vec![],
        labels: std::collections::BTreeMap::new(),
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
