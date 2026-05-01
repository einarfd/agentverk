//! VM lifecycle management — create, start, stop, destroy.
//!
//! This module orchestrates the high-level VM operations, delegating to
//! submodules for QEMU process management, cloud-init, and instance state.

pub mod cloud_init;
pub mod forwarding;
pub mod instance;
pub mod provision;
pub mod qemu;
pub mod system_info;
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

use serde::Serialize;

use crate::config::{MixinManualSteps, ResolvedConfig};
use crate::error::Error;
use crate::{dirs, idle_watcher, image, ssh, ssh_config};
use instance::{Instance, Phase, ProvisionState, Status};

/// Machine-readable snapshot of a VM's current state.
///
/// Returned by `agv create --json` (and, in the future, by `agv inspect
/// --json`). Stable over the 0.x minor series — additions are
/// backwards-compatible, removals/renames need a major bump.
#[derive(Debug, Clone, Serialize)]
pub struct VmStateReport {
    /// VM name (matches the instance directory).
    pub name: String,
    /// Status string: `creating` / `configuring` / `running` / `stopped` /
    /// `suspended` / `broken`.
    pub status: String,
    /// `true` when this report was produced by an `agv create` that
    /// actually created the VM; `false` when `--if-not-exists`
    /// short-circuited because the VM was already there.
    pub created: bool,
    /// SSH port on `127.0.0.1` (only present when status is `running`).
    pub ssh_port: Option<u16>,
    /// VM's default user (e.g. `agent`).
    pub user: String,
    /// Configured memory (e.g. `"8G"`).
    pub memory: String,
    /// Configured vCPU count.
    pub cpus: u32,
    /// Configured disk size (e.g. `"40G"`).
    pub disk: String,
    /// Mixins applied at create time, in the order they were merged.
    pub mixins_applied: Vec<String>,
    /// Per-mixin manual setup steps the human invoker still needs to do.
    /// Empty for VMs whose mixins all auto-configured.
    pub manual_steps: Vec<MixinManualSteps>,
    /// Top-level manual steps from the user's own config (VM-specific,
    /// not mixin-tagged).
    pub config_manual_steps: Vec<String>,
    /// Absolute path to the instance directory under
    /// `~/.local/share/agv/instances/`. Useful for agents that want to
    /// tail `provision.log` / `serial.log` for debugging.
    pub data_dir: String,

    /// Free-form key=value labels set at create time. Empty object when
    /// none were specified. agv stores them but doesn't interpret them
    /// — they're for callers to track which VMs they own (an agent's
    /// session, a human's hand-tagged distinguishing marks, etc.).
    pub labels: std::collections::BTreeMap<String, String>,

    /// Active port forwards (config-declared, ad-hoc, and auto-allocated).
    /// Empty array when no forwards are active. Each entry exposes
    /// `alive` so a stale forwards.toml entry whose supervisor died
    /// shows up clearly. Read without sweeping, so this is a snapshot
    /// of `<instance>/forwards.toml` plus per-PID liveness.
    pub forwards: Vec<crate::forward::ForwardJson>,

    /// Auto-suspend (idle-watcher) status. `null` when
    /// `idle_suspend_minutes == 0` (the default — auto-suspend not
    /// enabled). When the VM has it configured, the field carries the
    /// thresholds plus the watcher's PID and liveness so consumers can
    /// distinguish "configured + healthy" from "configured but watcher
    /// died" from "not configured."
    pub idle_suspend: Option<IdleSuspendStatus>,
}

/// Auto-suspend configuration and live watcher state, surfaced via
/// `VmStateReport::idle_suspend`. Stable shape across the 0.x series —
/// additions OK, removals/renames need a major bump.
#[derive(Debug, Clone, Serialize)]
pub struct IdleSuspendStatus {
    /// Configured `idle_suspend_minutes`. Always `> 0` when this struct
    /// is present (the parent field is `None` for the disabled case).
    pub minutes: u32,
    /// Configured `idle_load_threshold` (default `0.2`).
    pub load_threshold: f32,
    /// Watcher supervisor PID, or `null` if no `idle_watcher.pid` file
    /// is on disk (e.g. the watcher hasn't been spawned yet, or its
    /// pidfile was cleaned up after exit).
    pub watcher_pid: Option<u32>,
    /// Whether the PID above is still a running process. `false` when
    /// `watcher_pid` is `null` or when the recorded PID no longer
    /// exists — in either case the VM has auto-suspend configured but
    /// nothing is currently monitoring it.
    pub watcher_alive: bool,
}

/// JSON shape returned by `agv destroy --json`.
///
/// Intentionally distinct from `VmStateReport` because the VM no longer
/// exists — there's no instance dir to read state from. Consumers can
/// branch on the `destroyed` field, which is always `true` (any failure
/// surfaces as a non-zero exit before this is emitted).
#[derive(Debug, Clone, Serialize)]
pub struct DestroyReport {
    pub name: String,
    pub destroyed: bool,
}

/// Build a `VmStateReport` for an existing instance.
///
/// `created` distinguishes "I just created this VM" (true) from
/// "this VM was already there and I'm reporting its current state"
/// (false). Both cases produce the same shape; agents discriminate via
/// the `created` field.
pub async fn state_report(inst: &Instance, created: bool) -> anyhow::Result<VmStateReport> {
    let status = inst
        .reconcile_status()
        .await
        .map_or_else(|_| "unknown".to_string(), |s| s.to_string());

    let cfg = crate::config::load_resolved(&inst.config_path())?;

    // SSH port file is only present when QEMU is running.
    let ssh_port = match tokio::fs::read_to_string(inst.ssh_port_path()).await {
        Ok(raw) => raw.trim().parse::<u16>().ok(),
        Err(_) => None,
    };

    // Snapshot of active forwards. Read without sweeping — `inspect`
    // shouldn't mutate state files; let the consumer see stale entries
    // explicitly via `alive: false` if any.
    let forwards: Vec<crate::forward::ForwardJson> =
        match crate::forward::read_active(&inst.forwards_path()).await {
            Ok(active) => active.into_iter().map(Into::into).collect(),
            Err(_) => Vec::new(),
        };

    let idle_suspend = idle_suspend_status(inst, &cfg).await;

    Ok(VmStateReport {
        name: inst.name.clone(),
        status,
        created,
        ssh_port,
        user: cfg.user,
        memory: cfg.memory,
        cpus: cfg.cpus,
        disk: cfg.disk,
        mixins_applied: cfg.mixins_applied,
        manual_steps: cfg.mixin_manual_steps,
        config_manual_steps: cfg.config_manual_steps,
        data_dir: inst.dir.display().to_string(),
        labels: cfg.labels,
        forwards,
        idle_suspend,
    })
}

/// Render the auto-suspend section of `agv inspect` (human output).
/// No-op when auto-suspend is not configured.
async fn print_auto_suspend(inst: &Instance, config: &ResolvedConfig) {
    if config.idle_suspend_minutes == 0 {
        return;
    }
    let pid_raw = tokio::fs::read_to_string(inst.idle_watcher_pid_path())
        .await
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok());
    let watcher_state = match pid_raw {
        Some(pid) if crate::forward::is_alive(pid) => format!("pid {pid}, alive"),
        Some(pid) => format!("pid {pid}, dead"),
        None => "not running".to_string(),
    };
    println!();
    println!("  Auto-suspend");
    println!(
        "    after {min} idle min (5-min loadavg < {thr:.2})",
        min = config.idle_suspend_minutes,
        thr = config.idle_load_threshold,
    );
    println!("    watcher: {watcher_state}");
}

/// Build the `idle_suspend` field of `VmStateReport`.
///
/// Returns `None` when `idle_suspend_minutes == 0` (auto-suspend not
/// configured). Otherwise reads `<instance>/idle_watcher.pid` to fill
/// in `watcher_pid` and probes the PID for liveness — both the
/// "watcher hasn't started yet" and "watcher died" cases surface as
/// `watcher_alive: false`.
async fn idle_suspend_status(
    inst: &Instance,
    cfg: &ResolvedConfig,
) -> Option<IdleSuspendStatus> {
    if cfg.idle_suspend_minutes == 0 {
        return None;
    }
    let watcher_pid = match tokio::fs::read_to_string(inst.idle_watcher_pid_path()).await {
        Ok(raw) => raw.trim().parse::<u32>().ok(),
        Err(_) => None,
    };
    let watcher_alive = watcher_pid.is_some_and(crate::forward::is_alive);
    Some(IdleSuspendStatus {
        minutes: cfg.idle_suspend_minutes,
        load_threshold: cfg.idle_load_threshold,
        watcher_pid,
        watcher_alive,
    })
}

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

/// Spawn the per-VM idle watcher if `idle_suspend_minutes > 0` in the
/// resolved config.
///
/// Best-effort and non-fatal: on failure the VM is still up, the user
/// just doesn't get auto-suspend until the next start. Mirrors how
/// forward-supervisor failures are handled in
/// [`apply_and_report_forwards`].
async fn maybe_spawn_idle_watcher(inst: &Instance, config: &ResolvedConfig, spinner: &ProgressBar) {
    if config.idle_suspend_minutes == 0 {
        return;
    }
    idle_watcher::spawn(
        &inst.name,
        config.idle_suspend_minutes,
        config.idle_load_threshold,
    )
    .await;
    step_done(
        spinner,
        &format!(
            "Auto-suspend after {} idle minute{}",
            config.idle_suspend_minutes,
            if config.idle_suspend_minutes == 1 { "" } else { "s" }
        ),
    );
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
    force: bool,
) -> anyhow::Result<()> {
    // Guard: instance must not already exist.
    let inst_dir = dirs::instance_dir(name)?;
    if inst_dir.exists() {
        return Err(Error::VmAlreadyExists {
            name: name.to_string(),
        }
        .into());
    }

    // Pre-flight capacity check — only matters when we're about to boot.
    // `agv create` without `--start` doesn't allocate host RAM at all.
    if start_after {
        let new_memory = crate::image::parse_disk_size(&config.memory).unwrap_or(0);
        let host = crate::resources::probe_host(&dirs::data_dir()?)?;
        let allocated = crate::resources::probe_allocated(&list().await?).await?;
        crate::resources::check_capacity(new_memory, &host, &allocated, force)?;
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

    maybe_spawn_idle_watcher(inst, config, &spinner).await;

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

    maybe_spawn_idle_watcher(&inst, &config, &spinner).await;

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

    // Active port forwards (config-declared, ad-hoc, and auto-allocated).
    // Subsumes the older "<name> port" display for auto_forwards: the
    // friendly name is shown inline on the matching entry, and ad-hoc
    // and config forwards now show up too instead of needing a separate
    // `agv forward --list` invocation.
    if status == Status::Running {
        let active = crate::forward::read_active(&inst.forwards_path())
            .await
            .unwrap_or_default();
        if !active.is_empty() {
            // Map guest_port → declared auto_forward name so we can label
            // auto entries with their friendly name.
            let auto_names: std::collections::BTreeMap<u16, &str> = config
                .auto_forwards
                .iter()
                .map(|(n, af)| (af.guest_port, n.as_str()))
                .collect();
            println!("  Forwards");
            for entry in &active {
                let alive_marker = if crate::forward::is_alive(entry.pid) {
                    ""
                } else {
                    " [dead]"
                };
                let label = match entry.origin {
                    crate::forward::Origin::Auto => auto_names
                        .get(&entry.guest)
                        .map_or_else(|| "auto".to_string(), |n| format!("auto: {n}")),
                    crate::forward::Origin::Config => "config".to_string(),
                    crate::forward::Origin::Adhoc => "adhoc".to_string(),
                };
                println!(
                    "    127.0.0.1:{host} → guest:{guest}  ({label}){alive_marker}",
                    host = entry.host,
                    guest = entry.guest,
                );
            }
        }
    }

    let provisioned = if inst.is_provisioned() { "yes" } else { "no" };
    println!("  {:<w$}  {provisioned}", "Provisioned");
    println!("  {:<w$}  {}", "Data dir", inst.dir.display());

    print_auto_suspend(&inst, &config).await;

    // Labels — only print the section when there are any. Empty values
    // render as just the key (matches the `--label foo` shorthand for
    // `foo=""`).
    if !config.labels.is_empty() {
        println!();
        println!("  Labels");
        for (k, v) in &config.labels {
            if v.is_empty() {
                println!("    {k}");
            } else {
                println!("    {k}={v}");
            }
        }
    }

    // Surface manual setup steps the mixins / top-level config flagged.
    // These are imperative instructions for the human invoker (auth flows,
    // etc) — agv prints them on the first successful provision, but
    // re-surfaces them here so a user who closed that terminal can read
    // them again later. No tracking of "done"; the user re-reads as
    // needed.
    crate::manual_steps::print_to_host(&config);

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
    // a few seconds retrying against a dying SSH server. The idle watcher
    // gets the same treatment so it doesn't keep probing a stopping VM.
    idle_watcher::stop(&inst).await;
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
    // Idempotent: the watcher (when triggering this code path itself)
    // removes its own pid file before calling us, so this is a no-op
    // in the auto-suspend case and a real cleanup in the manual case.
    idle_watcher::stop(&inst).await;
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

    maybe_spawn_idle_watcher(&inst, &config, &spinner).await;

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
        idle_watcher::stop(&inst).await;
        forwarding::stop_all_for_instance(&inst).await;
        let _ = qemu::force_stop(&inst).await;
    } else {
        // Even on a stopped/broken VM, sweep any stale supervisors that
        // a previous run might have left in forwards.toml or the watcher
        // pid file.
        idle_watcher::stop(&inst).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> VmStateReport {
        let mut labels = std::collections::BTreeMap::new();
        labels.insert("session".to_string(), "abc123".to_string());
        labels.insert("task".to_string(), String::new());
        VmStateReport {
            name: "myvm".to_string(),
            status: "running".to_string(),
            created: true,
            ssh_port: Some(50001),
            user: "agent".to_string(),
            memory: "8G".to_string(),
            cpus: 4,
            disk: "40G".to_string(),
            mixins_applied: vec!["devtools".to_string(), "claude".to_string()],
            manual_steps: vec![MixinManualSteps {
                name: "claude".to_string(),
                steps: vec!["Run `claude /login`...".to_string()],
            }],
            config_manual_steps: vec!["Configure VPN before starting work.".to_string()],
            data_dir: "/Users/u/.local/share/agv/instances/myvm".to_string(),
            labels,
            forwards: vec![crate::forward::ForwardJson {
                host: 8080,
                guest: 8080,
                origin: crate::forward::Origin::Config,
                alive: true,
            }],
            idle_suspend: Some(IdleSuspendStatus {
                minutes: 30,
                load_threshold: 0.2,
                watcher_pid: Some(4242),
                watcher_alive: true,
            }),
        }
    }

    /// Pin the top-level JSON keys of `agv create --json` and
    /// `agv inspect --json` (when it lands). The CHANGELOG and audit
    /// promise this schema is stable across the 0.x series — additions
    /// OK, removals/renames are a major-version bump. This test exists
    /// to make a rename or removal fail loudly in CI.
    #[test]
    fn vm_state_report_json_schema_pin() {
        let report = fixture();
        let json = serde_json::to_value(&report).unwrap();
        let obj = json.as_object().expect("VmStateReport must serialize as a JSON object");

        // Sorted alphabetically so a removal lands on the same line as the
        // assertion that fails — easier to spot in a diff.
        let expected: &[&str] = &[
            "config_manual_steps",
            "cpus",
            "created",
            "data_dir",
            "disk",
            "forwards",
            "idle_suspend",
            "labels",
            "manual_steps",
            "memory",
            "mixins_applied",
            "name",
            "ssh_port",
            "status",
            "user",
        ];
        let actual: std::collections::BTreeSet<&str> = obj.keys().map(String::as_str).collect();
        let expected_set: std::collections::BTreeSet<&str> = expected.iter().copied().collect();

        let missing: Vec<&str> = expected_set.difference(&actual).copied().collect();
        assert!(
            missing.is_empty(),
            "VmStateReport JSON is missing expected keys (rename or removal? bump major): {missing:?}"
        );
        let unexpected: Vec<&str> = actual.difference(&expected_set).copied().collect();
        assert!(
            unexpected.is_empty(),
            "VmStateReport JSON has new keys not yet in the schema pin (add to the test): {unexpected:?}",
        );
    }

    /// Optional fields (`ssh_port`, `idle_suspend`) must round-trip as
    /// `null` when not set, not be omitted entirely. Agents parsing the
    /// JSON should be able to rely on every documented key being present.
    #[test]
    fn vm_state_report_omits_no_keys_for_stopped_vm() {
        let mut report = fixture();
        report.ssh_port = None;
        report.created = false;
        report.idle_suspend = None;
        let json = serde_json::to_value(&report).unwrap();
        let obj = json.as_object().unwrap();
        assert!(obj.contains_key("ssh_port"), "ssh_port must be in the object even when None");
        assert_eq!(obj.get("ssh_port"), Some(&serde_json::Value::Null));
        assert_eq!(obj.get("created"), Some(&serde_json::Value::Bool(false)));
        assert!(
            obj.contains_key("idle_suspend"),
            "idle_suspend must be in the object even when None"
        );
        assert_eq!(obj.get("idle_suspend"), Some(&serde_json::Value::Null));
    }

    /// Schema pin for `VmStateReport.idle_suspend` — drift here is also
    /// a major-version bump.
    #[test]
    fn idle_suspend_status_json_schema_pin() {
        let status = IdleSuspendStatus {
            minutes: 30,
            load_threshold: 0.2,
            watcher_pid: Some(4242),
            watcher_alive: true,
        };
        let json = serde_json::to_value(&status).unwrap();
        let obj = json.as_object().expect("IdleSuspendStatus must serialize as an object");
        let actual: std::collections::BTreeSet<&str> = obj.keys().map(String::as_str).collect();
        let expected: std::collections::BTreeSet<&str> =
            ["load_threshold", "minutes", "watcher_alive", "watcher_pid"]
                .into_iter()
                .collect();
        assert_eq!(actual, expected, "IdleSuspendStatus keys drifted");
    }

    /// `watcher_pid` must be `null` (not omitted) when the pid file is
    /// missing — same convention as the parent `idle_suspend` field.
    #[test]
    fn idle_suspend_status_serializes_null_pid() {
        let status = IdleSuspendStatus {
            minutes: 30,
            load_threshold: 0.2,
            watcher_pid: None,
            watcher_alive: false,
        };
        let json = serde_json::to_value(&status).unwrap();
        let obj = json.as_object().unwrap();
        assert!(obj.contains_key("watcher_pid"));
        assert_eq!(obj.get("watcher_pid"), Some(&serde_json::Value::Null));
    }

    /// `manual_steps` and `mixins_applied` must serialize as arrays
    /// (possibly empty), not be omitted. Agents iterate over them
    /// without first checking for presence.
    #[test]
    fn vm_state_report_collections_serialize_as_arrays() {
        let mut report = fixture();
        report.manual_steps = vec![];
        report.mixins_applied = vec![];
        report.config_manual_steps = vec![];
        let json = serde_json::to_value(&report).unwrap();
        let obj = json.as_object().unwrap();
        for key in ["manual_steps", "mixins_applied", "config_manual_steps"] {
            assert!(
                obj.get(key).is_some_and(serde_json::Value::is_array),
                "{key} should serialize as an array"
            );
        }
    }

    /// Empty labels must still serialize as `{}` (an empty object), not
    /// be omitted. Agents iterate / index into it without first checking
    /// for presence.
    #[test]
    fn vm_state_report_empty_labels_serialize_as_object() {
        let mut report = fixture();
        report.labels = std::collections::BTreeMap::new();
        let json = serde_json::to_value(&report).unwrap();
        let obj = json.as_object().unwrap();
        let labels = obj.get("labels").expect("labels key must be present even when empty");
        assert!(labels.is_object(), "labels must serialize as an object");
        assert!(labels.as_object().unwrap().is_empty());
    }

    /// Empty-string label values round-trip cleanly. `--label foo` with
    /// no `=` is shorthand for `foo=""`, and consumers should see exactly
    /// `""` in JSON, not the key being omitted.
    #[test]
    fn vm_state_report_empty_label_value_serializes_as_empty_string() {
        let report = fixture();  // fixture has "task" -> ""
        let json = serde_json::to_value(&report).unwrap();
        let labels = json.get("labels").unwrap().as_object().unwrap();
        assert_eq!(labels.get("task"), Some(&serde_json::Value::String(String::new())));
    }

    /// Schema pin for `agv destroy --json`. Same idea as the
    /// `VmStateReport` pin: a rename or removal of either field should
    /// fail loudly. Distinct shape from `VmStateReport` — destroy
    /// represents a VM that no longer exists.
    #[test]
    fn destroy_report_json_schema_pin() {
        let report = DestroyReport {
            name: "myvm".to_string(),
            destroyed: true,
        };
        let json = serde_json::to_value(&report).unwrap();
        let obj = json.as_object().expect("DestroyReport must serialize as an object");

        let actual: std::collections::BTreeSet<&str> = obj.keys().map(String::as_str).collect();
        let expected: std::collections::BTreeSet<&str> = ["destroyed", "name"].into_iter().collect();
        assert_eq!(actual, expected, "DestroyReport JSON keys drifted");
        assert_eq!(obj.get("destroyed"), Some(&serde_json::Value::Bool(true)));
    }
}

