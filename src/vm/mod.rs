//! VM lifecycle management — create, start, stop, destroy.
//!
//! This module orchestrates the high-level VM operations, delegating to
//! submodules for QEMU process management, cloud-init, and instance state.

pub mod backend;
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
    /// Backend the VM runs on: `"qemu"` (default, all platforms) or
    /// `"avf"` (Apple Virtualization, macOS-only). Set at create
    /// time; consumers can use it to render the right SSH endpoint
    /// (`ssh_port` is null on AVF; SSH goes through the guest IP).
    pub backend: String,
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

/// JSON shape returned by `agv backend migrate-to-avf --json`.
///
/// Stable over the 0.x minor series — additions OK, removals/renames
/// need a major bump.
#[derive(Debug, Clone, Serialize)]
pub struct MigrateToAvfReport {
    pub name: String,
    /// Path to the new sparse raw disk under the instance dir.
    pub raw_disk_path: String,
    /// Size of the raw disk in bytes (the qcow2's virtual size, post-grow).
    pub raw_disk_size_bytes: u64,
    /// Path to the original qcow2. Whether it still exists depends on
    /// `qcow2_disk_kept`.
    pub qcow2_disk_path: String,
    /// `true` when the qcow2 was preserved for one-step rollback;
    /// `false` when `--delete-qcow2` was passed.
    pub qcow2_disk_kept: bool,
}

/// JSON shape returned by `agv backend cleanup --json`. Lists the
/// previous-backend files agv would remove (or did remove) from a
/// VM's instance directory.
#[derive(Debug, Clone, Serialize)]
pub struct BackendCleanupReport {
    pub name: String,
    /// Current backend (the one whose files are kept).
    pub backend: String,
    /// Files removed, in deletion order. Absolute paths under the
    /// instance directory. Empty when there was nothing to clean.
    pub removed: Vec<RemovedFile>,
    /// Total bytes freed across `removed`.
    pub bytes_freed: u64,
    /// `true` when `--dry-run` was passed — `removed` then describes
    /// what *would* be deleted; the files are still on disk.
    pub dry_run: bool,
}

/// One entry in `BackendCleanupReport::removed`.
#[derive(Debug, Clone, Serialize)]
pub struct RemovedFile {
    pub path: String,
    pub bytes: u64,
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
        backend: cfg.backend,
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
/// Resolves the SSH endpoint through the backend trait so the entry is
/// correct for both QEMU (loopback + hostport) and AVF (guest NAT IP +
/// port 22). Called after `wait_for_ssh` succeeds, so the AVF runner
/// has a DHCP-assigned guest IP to report.
///
/// Best-effort — failures are logged but do not abort the operation.
async fn update_ssh_config(inst: &Instance, user: &str) {
    let backend = match backend::for_instance(inst) {
        Ok(b) => b,
        Err(e) => {
            warn!(vm = %inst.name, error = %format!("{e:#}"), "could not resolve backend for SSH config update");
            return;
        }
    };
    let (host, port) = match backend.ssh_endpoint(inst).await {
        Ok(pair) => pair,
        Err(e) => {
            warn!(vm = %inst.name, error = %format!("{e:#}"), "could not resolve SSH endpoint for managed SSH config");
            return;
        }
    };
    if let Err(e) =
        ssh_config::add_entry(&inst.name, &host, port, user, &inst.ssh_key_path()).await
    {
        warn!(vm = %inst.name, error = %format!("{e:#}"), "failed to update managed SSH config");
    }
}

/// Print a completed-step line above the spinner, keeping previous output visible.
/// Human label for a backend, for user-facing spinner / log messages.
/// Matches the value of `backend = "..."` in `agv.toml`.
fn backend_label(cfg: &ResolvedConfig) -> &'static str {
    match cfg.backend.as_str() {
        "avf" => "Apple Virtualization",
        // Default + qemu both render as "QEMU"; an unknown value
        // shouldn't get this far (config validation catches it).
        _ => "QEMU",
    }
}

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

/// Resolve and pin the QEMU machine type if the instance config doesn't
/// have one yet, then return the value to pass to QEMU.
///
/// On a first start (config has `machine_type = None`), we shell out to
/// `qemu-system-X -machine help` to pick the host QEMU's current default
/// version (e.g. `pc-q35-9.2`) and persist it back into the instance
/// config. From that point on every start uses the same `-machine`
/// value, so a brew/distro QEMU upgrade can't silently change the
/// guest's device topology underneath an existing snapshot.
///
/// Existing instances created before this field existed deserialize as
/// `None` and get auto-pinned the same way on their next start.
pub(super) async fn ensure_machine_type(
    inst: &Instance,
    config: &mut ResolvedConfig,
) -> anyhow::Result<String> {
    if let Some(existing) = config.machine_type.clone() {
        return Ok(existing);
    }
    let resolved = qemu::current_default_machine_type()?;
    info!(
        vm = %inst.name,
        machine_type = %resolved,
        "auto-pinning QEMU machine type for this VM"
    );
    config.machine_type = Some(resolved.clone());
    crate::config::save(config, &inst.config_path()).await?;
    Ok(resolved)
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

    // Local clone for the machine-type auto-pin: ensure_machine_type sets
    // the pinned value on this struct and re-saves, so the rest of the
    // function should see and use the post-pin config.
    let mut config = config.clone();

    // Save resolved config to instance dir so restarts / inspect can reload it.
    crate::config::save(&config, &inst.config_path()).await?;

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

    // Provision the per-instance disk in whatever format the chosen
    // backend wants (qcow2 overlay for QEMU, sparse raw for AVF).
    spinner.set_message(format!("Provisioning {} disk...", config.disk));
    info!(size = %config.disk, backend = %config.backend, "provisioning disk");
    backend::for_config(&config)
        .provision_disk(inst, &base_image, &config.disk)
        .await?;
    step_done(&spinner, &format!("Provisioned {} disk", config.disk));

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

    // Resolve and persist the QEMU machine type pin (auto-pinned on first
    // start; no-op once the value is recorded in the instance config).
    let machine_type = ensure_machine_type(inst, &mut config).await?;

    // Start the VM under the configured backend.
    let label = backend_label(&config);
    spinner.set_message(format!(
        "Starting {label} ({} RAM, {} vCPUs)...",
        config.memory, config.cpus
    ));
    info!(name, memory = %config.memory, cpus = config.cpus, backend = %config.backend, "starting VM");
    backend::for_config(&config)
        .start(inst, &config, &machine_type, None)
        .await?;
    inst.write_status(Status::Running).await?;
    step_done(
        &spinner,
        &format!("Started {label} ({} RAM, {} vCPUs)", config.memory, config.cpus),
    );

    // Run first-boot provisioning (wait for SSH, setup, provision).
    run_first_boot(inst, &config, interactive_mode, verbose, quiet, &spinner).await?;

    // Apply config-declared and auto-allocated forwards. Must run after
    // SSH is up (the supervisors tunnel through sshd). Same step runs in
    // `start` and `resume` — keeping it here means `agv create --start`
    // yields a VM with its forwards already live.
    apply_and_report_forwards(inst, &config, &spinner).await;

    maybe_spawn_idle_watcher(inst, &config, &spinner).await;

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
#[expect(
    clippy::too_many_arguments,
    reason = "one positional per knob; bundling them in a struct just shifts the boilerplate to the call site"
)]
pub async fn config_set(
    name: &str,
    memory: Option<&str>,
    cpus: Option<u32>,
    disk: Option<&str>,
    forwards: Option<&str>,
    idle_suspend_minutes: Option<u32>,
    idle_load_threshold: Option<f32>,
    machine_type: Option<&str>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        memory.is_some()
            || cpus.is_some()
            || disk.is_some()
            || forwards.is_some()
            || idle_suspend_minutes.is_some()
            || idle_load_threshold.is_some()
            || machine_type.is_some(),
        "no changes specified — provide at least one of --memory, --cpus, --disk, --forwards, --idle-suspend-minutes, --idle-load-threshold, --machine-type"
    );

    if let Some(t) = idle_load_threshold {
        anyhow::ensure!(
            t.is_finite() && t > 0.0,
            "--idle-load-threshold must be a positive finite number (got {t})"
        );
    }

    let inst = Instance::open(name)?;

    // Refuse to enable auto-suspend on an AVF VM. The idle watcher's
    // job is to trigger `vm::suspend`, which AVF refuses (Apple's
    // framework doesn't support save/restore for Linux guests). A
    // watcher there would just retry-and-fail forever in the logs.
    if let Some(m) = idle_suspend_minutes
        && m > 0
    {
        let cfg = crate::config::load_resolved(&inst.config_path())?;
        anyhow::ensure!(
            cfg.backend != "avf",
            "idle_suspend_minutes is not supported on the avf backend — \
             Apple Virtualization framework does not support save/restore \
             for Linux guests, so the idle watcher's auto-suspend would \
             always fail. Recreate the VM with `--backend qemu` if you \
             need auto-suspend."
        );
    }

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
    if let Some(m) = idle_suspend_minutes {
        config.idle_suspend_minutes = m;
    }
    if let Some(t) = idle_load_threshold {
        config.idle_load_threshold = t;
    }
    if let Some(mt) = machine_type {
        config.machine_type = Some(mt.to_string());
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

    // Handle --retry: VM must be broken. Provisioning can be in any
    // state — `--retry` either resumes provisioning (if incomplete)
    // or just re-attempts the boot/SSH wait (if complete). The
    // latter is the right escape hatch when a VM ends up broken
    // for reasons unrelated to provisioning — e.g. an AVF cold
    // boot after migration where wait_for_ssh timed out but the
    // disk is fully provisioned from its QEMU life.
    if retry {
        if status != Status::Broken {
            anyhow::bail!(
                "--retry requires VM '{name}' to be in broken state (currently {status})"
            );
        }
    } else {
        // Normal start: VM must be stopped, OR broken with the VM process
        // still running (in which case we tell the user to use --retry).
        if status == Status::Broken {
            anyhow::bail!(
                "VM '{name}' is broken. Use 'agv start --retry {name}' to \
                 retry (resumes provisioning if incomplete, or retries the \
                 boot if it is complete), or 'agv destroy {name}' to start \
                 over."
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

    let mut config = crate::config::load_resolved(&inst.config_path())?;

    let spinner = status_spinner(verbose, quiet);

    // Start QEMU only if it's not already running (a broken VM may still
    // have the VM process alive — the user wants to retry, not restart from scratch).
    let label = backend_label(&config);
    let already_running = retry && inst.is_process_alive().await;
    if already_running {
        step_done(&spinner, &format!("{label} already running — retrying provisioning"));
    } else {
        let machine_type = ensure_machine_type(&inst, &mut config).await?;
        spinner.set_message(format!(
            "Starting {label} ({} RAM, {} vCPUs)...",
            config.memory, config.cpus
        ));
        backend::for_config(&config)
            .start(&inst, &config, &machine_type, None)
            .await?;
        step_done(
            &spinner,
            &format!("Started {label} ({} RAM, {} vCPUs)", config.memory, config.cpus),
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
    println!("  {:<w$}  {}", "Backend", backend_label(&config));

    // SSH connection info — meaningful when running, or broken-but-SSH-came-up.
    // Only QEMU writes an ssh_port file (host-side port forward);
    // AVF VMs reach the guest via its NAT IP, surfaced as the
    // backend's `ssh_endpoint` rather than a fixed port.
    let ssh_might_work = status == Status::Running
        || (status == Status::Broken && provision_state.phase != Phase::SshWait);
    if ssh_might_work && config.backend != "avf" {
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
    let backend = backend::for_instance(&inst)?;
    if force {
        backend.force_stop(&inst).await?;
    } else {
        backend.stop(&inst).await?;
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
    // Refuse early for AVF VMs — Apple Virtualization framework does
    // not support save/restore for Linux guests as of macOS 15 / 26
    // (the runner itself also refuses, but checking here avoids tearing
    // down the idle watcher and port forwards before learning the VM
    // can't actually suspend). Re-enable once the framework lifts the
    // restriction; see `swift/avf-runner/Sources/avf-runner/main.swift`
    // `restoreAndResume` for the full root-cause notes.
    let cfg = crate::config::load_resolved(&inst.config_path())?;
    if cfg.backend == "avf" {
        anyhow::bail!(
            "suspend is not supported for VMs on the avf backend — \
             Apple Virtualization framework does not support save/restore \
             for Linux guests. Use `agv stop {name}` + `agv start {name}` \
             instead, or recreate the VM with `--backend qemu`."
        );
    }
    // Idempotent: the watcher (when triggering this code path itself)
    // removes its own pid file before calling us, so this is a no-op
    // in the auto-suspend case and a real cleanup in the manual case.
    idle_watcher::stop(&inst).await;
    forwarding::stop_all_for_instance(&inst).await;
    backend::for_instance(&inst)?.suspend(&inst).await?;
    inst.write_status(Status::Suspended).await?;
    let _ = ssh_config::remove_entry(name).await;
    Ok(())
}

/// Resume a suspended VM by restarting the VM process with the
/// saved snapshot.
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

    let mut config = crate::config::load_resolved(&inst.config_path())?;

    let spinner = status_spinner(verbose, quiet);
    spinner.set_message(format!(
        "Resuming VM ({} RAM, {} vCPUs)...",
        config.memory, config.cpus
    ));

    let machine_type = ensure_machine_type(&inst, &mut config).await?;
    backend::for_config(&config)
        .start(&inst, &config, &machine_type, Some("agv-suspend"))
        .await?;
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

/// Migrate a stopped VM from the QEMU backend to Apple Virtualization.
///
/// Pipeline:
///   1. VM must be stopped (suspended/running rejected — the snapshot
///      state captured by QEMU's savevm has a different format and
///      can't be carried across; users should resume + stop first).
///   2. Current backend must be `"qemu"`; flipping an already-AVF VM
///      is rejected as a no-op error to surface user mistakes.
///   3. Convert `disk.qcow2` → `disk.raw` via the same qcow2-rs
///      converter the AVF cold-boot uses. Output sparseness is
///      slightly less aggressive than `qemu-img convert` (see
///      `src/qcow2.rs`) — acceptable for a one-time migration.
///   4. Rewrite `<inst>/config.toml` with `backend = "avf"`.
///   5. Optionally delete the source qcow2.
///
/// The MAC and machine-id sidecars get created on first AVF boot by
/// the runner — no need to pre-generate them. The AVF EFI variable
/// store is also created on first boot, so we leave the QEMU
/// `efi-vars.fd` in place rather than convert (incompatible
/// formats; the AVF backend reads its own
/// `avf-efi-vars.bin` and ignores `efi-vars.fd`).
///
/// macOS-only. The function returns an error on Linux because the
/// `avf` backend can't be selected there.
#[expect(
    clippy::too_many_lines,
    reason = "Migration is a sequential transactional pipeline (validate → convert disk → bump instance-id → switch backend field → save config → optionally delete qcow2 → emit report). Each step has rollback-on-failure semantics that depend on the surrounding state; pulling them into helpers would scatter the rollback logic and make the failure path harder to verify."
)]
#[cfg_attr(
    not(target_os = "macos"),
    expect(
        clippy::unused_async,
        reason = "Non-macOS bodies are a stub `anyhow::bail!`; keeping the `async fn` signature uniform across platforms so the dispatch site doesn't need a cfg-cascade. The macOS body uses `.await`."
    )
)]
pub async fn migrate_to_avf(
    name: &str,
    delete_qcow2: bool,
) -> anyhow::Result<MigrateToAvfReport> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (name, delete_qcow2);
        anyhow::bail!(
            "`agv backend migrate-to-avf` is macOS-only — Apple Virtualization is not available on this platform"
        );
    }
    #[cfg(target_os = "macos")]
    {
        let inst = Instance::open(name)?;
        let status = inst.reconcile_status().await?;
        anyhow::ensure!(
            status == Status::Stopped,
            Error::VmBadState {
                name: name.to_string(),
                status: status.to_string(),
                expected: "stopped".to_string(),
            }
        );

        let mut cfg = crate::config::load_resolved(&inst.config_path())?;
        anyhow::ensure!(
            cfg.backend != "avf",
            "VM '{name}' is already on the AVF backend — nothing to migrate"
        );

        // Fail-fast: confirm the AVF runner binary is locatable
        // BEFORE we do anything destructive. Without this, a user
        // who hasn't installed agv-avf-runner alongside agv (e.g.
        // a dev `cargo install` without the runner) would get
        // through conversion + config flip and only see the
        // problem on `agv start`, by which point rollback means
        // re-running the converter or manually flipping the
        // config back.
        let _runner = backend::locate_avf_runner().with_context(|| {
            format!(
                "cannot migrate '{name}' to AVF — the agv-avf-runner binary isn't \
                 findable. Run `just build-avf-runner` and either set \
                 AGV_AVF_RUNNER, or install agv-avf-runner alongside agv \
                 (`{}`)",
                std::env::current_exe()
                    .map(|p| p.parent().map(|p| p.display().to_string()).unwrap_or_default())
                    .unwrap_or_default(),
            )
        })?;

        let qcow2 = inst.disk_path();
        let raw = inst.avf_disk_path();
        anyhow::ensure!(
            qcow2.exists(),
            "source disk {} doesn't exist — VM may be corrupted",
            qcow2.display()
        );
        anyhow::ensure!(
            !raw.exists(),
            "destination raw disk {} already exists — refusing to overwrite",
            raw.display()
        );

        info!(vm = name, "converting qcow2 → sparse raw");
        // Long-running step on multi-GiB disks (typically 5–30s for
        // a 10G image). Render a spinner so the user knows the
        // command isn't hung — the underlying converter doesn't
        // expose progress so we can only show "still working."
        let spinner = status_spinner(false, false);
        spinner.set_message(format!(
            "Converting disk image (qcow2 → raw) — {}",
            qcow2.display()
        ));
        let result = crate::qcow2::convert_to_sparse_raw(&qcow2, &raw).await;
        if let Err(e) = result {
            spinner.finish_and_clear();
            return Err(e).with_context(|| {
                format!("converting {} → {}", qcow2.display(), raw.display())
            });
        }
        step_done(&spinner, "Converted disk image to sparse raw");

        let raw_size = tokio::fs::metadata(&raw)
            .await
            .with_context(|| format!("stat {}", raw.display()))?
            .len();

        // Regenerate the cloud-init seed with a fresh instance-id.
        // Without this, the QEMU-era cloud-init networking config
        // sticks to the disk and the guest never brings up its NIC
        // on the AVF NAT (different virtual hardware, no DHCP
        // request → runner's status RPC never reports a guest_ip
        // → `agv ssh` can't reach the migrated VM). Bumping the
        // instance-id is the cloud-init-native way to say "treat
        // this boot as a new instance and re-run init."
        let pub_key_path = inst.ssh_pub_key_path();
        let pub_key = tokio::fs::read_to_string(&pub_key_path)
            .await
            .with_context(|| {
                format!(
                    "reading {} (needed to regenerate the cloud-init seed)",
                    pub_key_path.display()
                )
            })?;
        let migration_instance_id = format!("{}-avf-migrated", inst.name);
        cloud_init::generate_seed_with_instance_id(
            &inst.seed_path(),
            pub_key.trim(),
            &inst.name,
            &migration_instance_id,
            &cfg.user,
        )
        .await
        .context("regenerating cloud-init seed for AVF migration")?;

        // Flip the config to backend=avf. Reuse the existing
        // config persistence path so the on-disk TOML is rewritten
        // through the same code that handles any other field.
        cfg.backend = "avf".to_string();
        crate::config::save(&cfg, &inst.config_path())
            .await
            .with_context(|| format!("writing {}", inst.config_path().display()))?;

        let kept = if delete_qcow2 {
            tokio::fs::remove_file(&qcow2)
                .await
                .with_context(|| format!("removing {}", qcow2.display()))?;
            false
        } else {
            true
        };

        info!(
            vm = name,
            kept_qcow2 = kept,
            raw_size_bytes = raw_size,
            "AVF migration complete"
        );

        Ok(MigrateToAvfReport {
            name: name.to_string(),
            raw_disk_path: raw.display().to_string(),
            raw_disk_size_bytes: raw_size,
            qcow2_disk_path: qcow2.display().to_string(),
            qcow2_disk_kept: kept,
        })
    }
}

/// Files that belong exclusively to the QEMU backend. When the VM
/// has flipped to `backend = "avf"`, anything still on disk under
/// these names is residue from before the flip and can be removed.
///
/// `pid` / `qmp.sock` are runtime-only (cleaned up at stop) and
/// rarely present, but listing them here makes the cleanup
/// idempotent if a previous run crashed before the runtime sweep
/// finished.
fn qemu_residue_files(inst: &Instance) -> Vec<std::path::PathBuf> {
    vec![
        inst.disk_path(),
        inst.efi_vars_path(),
        inst.pid_path(),
        inst.qmp_socket_path(),
        inst.ssh_port_path(),
    ]
}

/// Files that belong exclusively to the AVF backend. The mirror
/// of [`qemu_residue_files`] — removed when the VM has been
/// flipped back to `backend = "qemu"` (a flow that doesn't exist
/// today as a built-in, but the cleanup is symmetric for
/// completeness and matches the doc on `agv backend cleanup`).
fn avf_residue_files(inst: &Instance) -> Vec<std::path::PathBuf> {
    vec![
        inst.avf_disk_path(),
        inst.avf_runner_pid_path(),
        inst.avf_runner_config_path(),
        inst.avf_control_socket_path(),
        inst.avf_efi_vars_path(),
        inst.avf_snapshot_path(),
        inst.avf_mac_path(),
        inst.avf_machine_id_path(),
        // The runner log is informational, but it's per-backend
        // and grows on every boot — sweep it too.
        inst.dir.join("avf-runner.log"),
    ]
}

/// Sweep residual files from the previous backend.
///
/// Refuses to do anything if the host VM process is alive — the
/// running backend might be writing to one of these files. Otherwise
/// stats each file in the opposite backend's residue list, sums the
/// sizes for the report, and (unless `dry_run`) removes them.
///
/// Bidirectional even though only `migrate-to-avf` exists today:
/// keeps the command symmetric if the reverse migration ever lands,
/// and means a hand-edited `backend = "qemu"` flip still gets the
/// expected sweep.
pub async fn backend_cleanup(name: &str, dry_run: bool) -> anyhow::Result<BackendCleanupReport> {
    let inst = Instance::open(name)?;
    let status = inst.reconcile_status().await?;
    anyhow::ensure!(
        status != Status::Running,
        Error::VmBadState {
            name: name.to_string(),
            status: status.to_string(),
            expected: "stopped, suspended, or broken (not running)".to_string(),
        }
    );
    // Belt and braces — `reconcile_status` flips a stale `running`
    // to `stopped`, but a `broken` VM deliberately keeps its host
    // process alive for debugging. Refuse there too: deleting
    // disk.raw out from under a live AVF runner would be bad.
    anyhow::ensure!(
        !inst.is_process_alive().await,
        "VM '{name}' has a live host process — stop or destroy it before running cleanup"
    );

    let cfg = crate::config::load_resolved(&inst.config_path())?;
    let targets = match cfg.backend.as_str() {
        "avf" => qemu_residue_files(&inst),
        // Everything else (qemu or any unrecognised value that
        // config validation hasn't already rejected) — sweep the
        // AVF side.
        _ => avf_residue_files(&inst),
    };

    let mut removed = Vec::new();
    let mut bytes_freed: u64 = 0;
    for path in targets {
        // Stat first — missing files are the common case and
        // shouldn't appear in the report.
        let Ok(meta) = tokio::fs::metadata(&path).await else {
            continue;
        };
        let bytes = meta.len();
        if !dry_run {
            // For symlinks / regular files / FIFOs use remove_file;
            // a socket file like avf-control.sock counts as a
            // "file" for remove_file purposes.
            tokio::fs::remove_file(&path).await.with_context(|| {
                format!("removing {}", path.display())
            })?;
        }
        bytes_freed += bytes;
        removed.push(RemovedFile {
            path: path.display().to_string(),
            bytes,
        });
    }

    Ok(BackendCleanupReport {
        name: name.to_string(),
        backend: cfg.backend,
        removed,
        bytes_freed,
        dry_run,
    })
}

pub async fn destroy(name: &str, force: bool) -> anyhow::Result<()> {
    let inst = Instance::open(name)?;
    let status = inst.reconcile_status().await?;

    if status == Status::Running {
        anyhow::ensure!(
            force,
            "VM '{name}' is running — stop it first, or pass --force to destroy it anyway"
        );
    }

    idle_watcher::stop(&inst).await;
    forwarding::stop_all_for_instance(&inst).await;

    // A "broken" VM deliberately keeps its host process alive so the user
    // can SSH in to debug, and for AVF the runner is `mem::forget`'d so it
    // survives the parent that spawned it. Either way: if anything's still
    // running, kill it before we nuke the instance dir, otherwise we
    // orphan the process (especially painful for AVF — the runner holds
    // the VZ VM open and there's no pid file left to find it from).
    if inst.is_process_alive().await {
        if let Ok(backend) = backend::for_instance(&inst) {
            let _ = backend.force_stop(&inst).await;
        }
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

    #[tokio::test]
    async fn config_set_requires_at_least_one_flag() {
        let err = config_set("nonexistent-vm", None, None, None, None, None, None, None)
            .await
            .expect_err("config_set with no flags should fail");
        let msg = format!("{err}");
        assert!(
            msg.contains("no changes specified"),
            "unexpected error message: {msg}"
        );
    }

    #[tokio::test]
    async fn config_set_rejects_zero_idle_load_threshold() {
        // Validation runs before any filesystem access, so a fake VM name is fine.
        let err = config_set("nonexistent-vm", None, None, None, None, None, Some(0.0), None)
            .await
            .expect_err("zero load threshold should fail validation");
        let msg = format!("{err}");
        assert!(
            msg.contains("must be a positive finite number"),
            "unexpected error message: {msg}"
        );
    }

    #[tokio::test]
    async fn config_set_rejects_negative_idle_load_threshold() {
        let err = config_set("nonexistent-vm", None, None, None, None, None, Some(-0.5), None)
            .await
            .expect_err("negative load threshold should fail validation");
        let msg = format!("{err}");
        assert!(
            msg.contains("must be a positive finite number"),
            "unexpected error message: {msg}"
        );
    }

    #[tokio::test]
    async fn config_set_rejects_nan_idle_load_threshold() {
        let err = config_set(
            "nonexistent-vm",
            None,
            None,
            None,
            None,
            None,
            Some(f32::NAN),
            None,
        )
        .await
        .expect_err("NaN load threshold should fail validation");
        let msg = format!("{err}");
        assert!(
            msg.contains("must be a positive finite number"),
            "unexpected error message: {msg}"
        );
    }

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
            backend: "qemu".to_string(),
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
            "backend",
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

