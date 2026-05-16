//! Pluggable VM execution backend.
//!
//! The trait isolates the lifecycle calls that differ between hypervisors:
//! today QEMU (everywhere) and Apple Virtualization (AVF, macOS only).
//! Everything above the boundary (cloud-init, SSH, mixins, port forwards,
//! idle watcher) stays backend-agnostic and uses the trait through
//! `&dyn VmBackend`.

use async_trait::async_trait;

use crate::config::ResolvedConfig;
use crate::vm::instance::Instance;
use crate::vm::qemu;

#[cfg(target_os = "macos")]
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use anyhow::{bail, Context as _};
#[cfg(target_os = "macos")]
use serde::{Deserialize, Serialize};
#[cfg(target_os = "macos")]
use std::os::unix::process::CommandExt as _;
#[cfg(target_os = "macos")]
use std::process::Stdio;
#[cfg(target_os = "macos")]
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
#[cfg(target_os = "macos")]
use tokio::net::UnixStream;
#[cfg(target_os = "macos")]
use std::time::Duration;
#[cfg(target_os = "macos")]
use tracing::{debug, info, warn};

/// Wire-protocol version agv expects from `agv-avf-runner`.
///
/// Must match `RUNNER_PROTOCOL_VERSION` in
/// `swift/avf-runner/Sources/avf-runner/main.swift`. Bump on any
/// change that affects the runner ↔ agv contract or observable
/// behaviour:
/// - Adding / removing / renaming fields in [`AvfRunnerConfig`]
///   (or the Swift `RunnerConfig`)
/// - Adding / removing / renaming control-socket ops or
///   [`AvfRpcResponse`] fields
/// - Changing the *semantics* of an existing op or field even if
///   the wire shape is unchanged (e.g. `stop` used to do ACPI-only
///   shutdown and now SIGKILL-escalates — bump)
/// - Fixing a runner bug that changes what agv observes
/// - Adding a new capability the runner advertises
///
/// Don't bump for pure refactors with no observable change.
///
/// Increment by 1 each time. There is no semver, no compatibility
/// range — install-skew is the only failure mode we guard against,
/// and the runner refuses to boot on mismatch (strict equality,
/// fail-fast with a clear install hint).
///
/// When bumping: update this constant **and** the matching
/// constant in main.swift in the *same commit*, and mention the
/// reason in the commit message. See `AGENTS.md` →
/// "Runner ↔ agv wire-protocol versioning".
#[cfg(target_os = "macos")]
pub const RUNNER_PROTOCOL_VERSION: u32 = 1;

/// Backends own VM lifecycle: boot, stop, suspend/resume, and the SSH
/// endpoint of the guest.
///
/// Methods are designed so the caller doesn't need to know whether QEMU's
/// `hostfwd` model or AVF's NAT-IP model is in use — `ssh_endpoint`
/// abstracts the difference. `start` takes the resolved config plus a
/// `machine_type` (only QEMU uses it; AVF will ignore the parameter).
#[async_trait]
pub trait VmBackend: Send + Sync {
    /// Materialize the per-instance disk from a base image.
    ///
    /// Called once at create time (and from the template clone path).
    /// Each backend chooses its own on-disk format and target path:
    /// QEMU produces a qcow2 overlay backed by `base_image` at
    /// `inst.disk_path()`; AVF converts the qcow2 to a sparse raw at
    /// `inst.avf_disk_path()` and grows it to `size`. Both are
    /// idempotent — re-running over an existing target is allowed
    /// (used by the migrate-to-avf flow).
    async fn provision_disk(
        &self,
        inst: &Instance,
        base_image: &std::path::Path,
        size: &str,
    ) -> anyhow::Result<()>;

    /// Boot the VM. If `loadvm` is `Some(name)`, restore from that
    /// snapshot rather than cold-booting.
    async fn start(
        &self,
        inst: &Instance,
        cfg: &ResolvedConfig,
        machine_type: &str,
        loadvm: Option<&str>,
    ) -> anyhow::Result<()>;

    /// Graceful shutdown (ACPI power button equivalent). Falls back to
    /// `force_stop` if the guest doesn't shut down within the backend's
    /// timeout.
    async fn stop(&self, inst: &Instance) -> anyhow::Result<()>;

    /// Force-kill the hypervisor process. Idempotent; returns `Ok(())`
    /// if there's nothing to kill.
    async fn force_stop(&self, inst: &Instance) -> anyhow::Result<()>;

    /// Save full VM state and exit the hypervisor. Resume is via
    /// `start(.., loadvm = Some(...))`.
    async fn suspend(&self, inst: &Instance) -> anyhow::Result<()>;

    /// SSH endpoint for connecting to the running guest as `(host, port)`.
    ///
    /// QEMU returns `("127.0.0.1", hostfwd_port)` reading the per-VM port
    /// allocated at boot time. AVF will return `(guest_nat_ip, 22)` — its
    /// NAT bridge is a real interface on the host so direct connections
    /// work without a `hostfwd`.
    async fn ssh_endpoint(&self, inst: &Instance) -> anyhow::Result<(String, u16)>;
}

/// Backend that runs the VM as a local QEMU process.
///
/// Today's default and only impl. Delegates straight to the
/// [`crate::vm::qemu`] module; preserves all current behavior including
/// machine-type pinning, the `-loadvm agv-suspend` resume path, and the
/// `127.0.0.1:hostfwd_port` SSH endpoint.
pub struct LocalQemuBackend;

/// Singleton backend instances.
static LOCAL_QEMU: LocalQemuBackend = LocalQemuBackend;
#[cfg(target_os = "macos")]
static LOCAL_AVF: LocalAvfBackend = LocalAvfBackend;

/// Pick the backend for a VM by inspecting its resolved config.
///
/// The config field is validated at load time
/// ([`crate::config::load_resolved`]) — `"qemu"` is always valid;
/// `"avf"` is valid on macOS only — so this function is infallible.
#[must_use]
pub fn for_config(cfg: &ResolvedConfig) -> &'static dyn VmBackend {
    match cfg.backend.as_str() {
        "qemu" => &LOCAL_QEMU,
        #[cfg(target_os = "macos")]
        "avf" => &LOCAL_AVF,
        // load_resolved rejects anything else, so this is unreachable
        // in production. Fall back to QEMU defensively rather than
        // panicking — wrong-but-safe beats a crash.
        _ => &LOCAL_QEMU,
    }
}

/// Convenience wrapper for call sites that have an [`Instance`] but
/// not its loaded config (e.g. SSH ops, the forward supervisor).
/// Reads `<instance>/config.toml` synchronously to determine which
/// backend the VM uses.
///
/// The disk read is cheap relative to the work the caller is about to
/// do (spawn ssh, scp, etc.); no caching today.
pub fn for_instance(inst: &Instance) -> anyhow::Result<&'static dyn VmBackend> {
    let cfg = crate::config::load_resolved(&inst.config_path())?;
    Ok(for_config(&cfg))
}

#[async_trait]
impl VmBackend for LocalQemuBackend {
    async fn provision_disk(
        &self,
        inst: &Instance,
        base_image: &std::path::Path,
        size: &str,
    ) -> anyhow::Result<()> {
        crate::image::create_overlay(base_image, &inst.disk_path(), size).await
    }

    async fn start(
        &self,
        inst: &Instance,
        cfg: &ResolvedConfig,
        machine_type: &str,
        loadvm: Option<&str>,
    ) -> anyhow::Result<()> {
        qemu::start_with_loadvm(inst, &cfg.memory, cfg.cpus, machine_type, loadvm).await
    }

    async fn stop(&self, inst: &Instance) -> anyhow::Result<()> {
        qemu::stop(inst).await
    }

    async fn force_stop(&self, inst: &Instance) -> anyhow::Result<()> {
        qemu::force_stop(inst).await
    }

    async fn suspend(&self, inst: &Instance) -> anyhow::Result<()> {
        qemu::suspend(inst).await
    }

    async fn ssh_endpoint(&self, inst: &Instance) -> anyhow::Result<(String, u16)> {
        let port = crate::ssh::ssh_port(inst).await?;
        Ok(("127.0.0.1".to_string(), port))
    }
}

// ---------------------------------------------------------------------------
// Apple Virtualization backend (macOS only)
// ---------------------------------------------------------------------------

/// Backend that runs the VM under Apple Virtualization (`Virtualization`
/// framework) via the `agv-avf-runner` Swift helper binary.
///
/// One runner process per VM, spawned at start time and controlled
/// through a per-VM unix socket protocol (line-delimited JSON-RPC; see
/// the runner's `ControlRequest` / `ControlResponse` types for the
/// wire shape).
///
/// Skeleton commit: every method is currently a "not yet implemented"
/// error. Fills land in subsequent commits as the runner spawn,
/// disk-format conversion, and JSON-RPC client wire up.
#[cfg(target_os = "macos")]
pub struct LocalAvfBackend;

#[cfg(target_os = "macos")]
#[async_trait]
impl VmBackend for LocalAvfBackend {
    /// Convert the cached qcow2 base image to a sparse raw under the
    /// instance directory, then grow it to the user-requested
    /// `size`. AVF doesn't support qcow2 directly; the raw is what
    /// `VZDiskImageStorageDeviceAttachment` opens.
    ///
    /// Idempotent — if the raw already exists at the right size we
    /// no-op.
    async fn provision_disk(
        &self,
        inst: &Instance,
        base_image: &std::path::Path,
        size: &str,
    ) -> anyhow::Result<()> {
        let dest = inst.avf_disk_path();
        let target_bytes = crate::image::parse_disk_size(size)?;
        if let Ok(meta) = tokio::fs::metadata(&dest).await
            && meta.len() == target_bytes
        {
            return Ok(());
        }

        // Cache the qcow2 → raw conversion in the image cache so
        // subsequent AVF VMs from the same base skip the multi-
        // second decode. The cached raw lives at
        // `<base>.qcow2.raw`; `clone_to` (cp -c, i.e. clonefile(2))
        // gives us a per-instance disk that shares APFS extents
        // with the cache copy-on-write — zero bytes copied,
        // independent file handles, and either side can be deleted
        // without affecting the other.
        let cached_raw = crate::raw_cache::ensure_cached_raw(base_image)
            .await
            .with_context(|| {
                format!("populating raw cache for {}", base_image.display())
            })?;
        crate::raw_cache::clone_to(&cached_raw, &dest)
            .await
            .with_context(|| {
                format!(
                    "cloning {} → {}",
                    cached_raw.display(),
                    dest.display()
                )
            })?;

        // The cached raw matches the qcow2's virtual size (e.g. 3
        // GiB for stock cloud images); grow the clone to the
        // user-spec'd size so the guest's growpart/resize2fs can
        // take advantage of it. The extension is sparse — no bytes
        // written, and the COW relationship with the cache is
        // unaffected.
        let f = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&dest)
            .await
            .with_context(|| format!("opening {} for resize", dest.display()))?;
        f.set_len(target_bytes)
            .await
            .with_context(|| format!("growing {} to {} bytes", dest.display(), target_bytes))?;
        Ok(())
    }

    /// Boot the VM under Apple Virtualization.
    ///
    /// Sequence:
    ///   1. Serialize the runner's JSON config to
    ///      `<instance>/avf-runner-config.json`. If `loadvm` is set
    ///      (i.e. the user ran `agv resume`), include
    ///      `restore_on_boot: true` so the runner restores the
    ///      machine state from `<instance>/avf-snapshot.bin`.
    ///   2. Locate the agv-avf-runner binary (env override → sibling
    ///      of current agv binary).
    ///   3. Spawn the runner detached, in its own process group, with
    ///      stderr captured to `<instance>/avf-runner.log` for
    ///      post-mortem debugging.
    ///   4. Persist the runner's PID for stop/destroy cleanup.
    ///   5. Poll the runner's control socket until state goes from
    ///      `starting` to `running`. If the runner dies during boot
    ///      or doesn't reach `running` in time, kill its process
    ///      group and surface an error.
    ///
    /// `machine_type` is ignored — AVF picks its own platform config.
    /// `loadvm`'s value is also ignored beyond `is_some()` — AVF
    /// has one snapshot per VM, not named snapshots like QEMU.
    async fn start(
        &self,
        inst: &Instance,
        cfg: &ResolvedConfig,
        _machine_type: &str,
        loadvm: Option<&str>,
    ) -> anyhow::Result<()> {
        let restore_on_boot = loadvm.is_some();
        if restore_on_boot && !inst.avf_snapshot_path().exists() {
            bail!(
                "cannot resume: AVF snapshot file {} is missing",
                inst.avf_snapshot_path().display()
            );
        }

        // Compose the JSON config the runner reads.
        let runner_cfg = AvfRunnerConfig {
            runner_protocol_version: RUNNER_PROTOCOL_VERSION,
            name: inst.name.clone(),
            memory_bytes: parse_memory(&cfg.memory)?,
            cpu_count: cfg.cpus,
            disk_path: inst.avf_disk_path().display().to_string(),
            seed_iso_path: inst.seed_path().display().to_string(),
            efi_variable_store_path: inst.avf_efi_vars_path().display().to_string(),
            serial_log_path: inst.serial_log_path().display().to_string(),
            control_socket_path: inst.avf_control_socket_path().display().to_string(),
            snapshot_path: inst.avf_snapshot_path().display().to_string(),
            restore_on_boot,
            mac_address_path: inst.avf_mac_path().display().to_string(),
            machine_identifier_path: inst.avf_machine_id_path().display().to_string(),
        };
        let cfg_path = inst.avf_runner_config_path();
        let cfg_json = serde_json::to_vec_pretty(&runner_cfg)
            .context("serializing AVF runner config")?;
        tokio::fs::write(&cfg_path, &cfg_json)
            .await
            .with_context(|| format!("writing {}", cfg_path.display()))?;

        // Stale socket from a previous unclean exit would cause the
        // runner to fail bind() — clean up first.
        let _ = tokio::fs::remove_file(inst.avf_control_socket_path()).await;

        let binary = locate_avf_runner()?;
        info!(
            vm = %inst.name,
            runner = %binary.display(),
            "spawning agv-avf-runner"
        );

        let log_path = inst.dir.join("avf-runner.log");
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("opening {}", log_path.display()))?;
        let log_clone = log_file.try_clone().context("dup runner log fd")?;

        let mut cmd = std::process::Command::new(&binary);
        cmd.arg("--config")
            .arg(&cfg_path)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_clone));
        cmd.process_group(0);

        let child = cmd
            .spawn()
            .with_context(|| format!("spawning {}", binary.display()))?;
        let pid = child.id();
        // Hand the runner off to the OS — same pattern as forward
        // supervisors and the idle watcher. The runner's lifetime is
        // managed via SIGTERM to the recorded PID, not a Rust handle.
        std::mem::forget(child);

        // Persist the PID so stop/destroy cleanup can find this
        // runner by name later.
        tokio::fs::write(inst.avf_runner_pid_path(), pid.to_string())
            .await
            .with_context(|| {
                format!(
                    "writing PID file {}",
                    inst.avf_runner_pid_path().display()
                )
            })?;

        // Wait for the runner to bind its control socket. If it fails
        // before that (bad config, validate() error, missing
        // entitlement) the process exits and we surface the runner's
        // log content inline so the user doesn't have to go digging
        // through the instance dir to find out what went wrong.
        match wait_for_avf_socket(inst, pid, Duration::from_secs(10)).await {
            Ok(()) => {}
            Err(e) => {
                avf_signal_runner(pid, rustix::process::Signal::TERM);
                let _ = tokio::fs::remove_file(inst.avf_runner_pid_path()).await;
                return Err(e.context(runner_failure_message(
                    "agv-avf-runner failed to start",
                    &log_path,
                )));
            }
        }

        // Now poll status until the VM reports `running`. AVF goes
        // through `starting → running` quickly on Apple Silicon
        // (typically under 2s), but the slow tests have shown cold
        // boots can take longer when the host is busy.
        match wait_for_avf_running(inst, Duration::from_secs(30)).await {
            Ok(()) => {}
            Err(e) => {
                avf_signal_runner(pid, rustix::process::Signal::TERM);
                let _ = tokio::fs::remove_file(inst.avf_runner_pid_path()).await;
                return Err(e.context(runner_failure_message(
                    "agv-avf-runner did not reach running state",
                    &log_path,
                )));
            }
        }

        debug!(vm = %inst.name, pid, "agv-avf-runner running");
        Ok(())
    }

    /// Send `{"op":"stop"}` over the runner's control socket, then
    /// wait for the runner process to exit. The runner schedules an
    /// ACPI shutdown asynchronously and returns `ok` immediately —
    /// but `stop` is contractually synchronous (the QEMU backend
    /// blocks until QEMU exits), so we mirror that here. Without
    /// the wait, the next `agv start` would race a still-alive
    /// runner pointing at the stale `avf-runner.pid` file.
    ///
    /// ACPI shutdown on a quiet Linux guest is a few seconds; we
    /// allow 30s so a busy guest with active fsync work has time to
    /// finish. If the runner overruns, fall back to `SIGTERM`ing its
    /// process group and waiting again. PID file is removed on
    /// successful exit so a future start has a clean slate.
    async fn stop(&self, inst: &Instance) -> anyhow::Result<()> {
        let pid = read_avf_runner_pid(inst).await;
        // Fire the RPC even if we don't have a PID — the runner may
        // be alive without us having recorded the PID (e.g. user
        // hand-restored a state dir). Loss of PID just means we
        // can't fall back to SIGTERM/SIGKILL afterwards.
        //
        // The RPC itself can fail when the runner is wedged (socket
        // present but unresponsive). Treat that as a soft failure if
        // we have a PID — the SIGTERM/SIGKILL escalation below will
        // recover. Without a PID there's nothing else to try.
        let rpc_result = avf_rpc(&inst.avf_control_socket_path(), "stop").await;
        if rpc_result.is_err() && pid.is_none() {
            rpc_result?;
        }

        if let Some(pid) = pid {
            if wait_for_pid_exit(pid, Duration::from_secs(30)).await.is_err() {
                warn!(
                    pid,
                    "agv-avf-runner didn't exit within 30s after stop RPC; escalating signals"
                );
                avf_terminate_runner(pid).await?;
            }
        }
        let _ = tokio::fs::remove_file(inst.avf_runner_pid_path()).await;
        Ok(())
    }

    /// Send `{"op":"force_stop"}` (abrupt VM stop via `vm.stop()`)
    /// and wait for the runner to exit. Falls back to SIGTERM if
    /// the runner is unresponsive on the control socket. Short
    /// timeout — a forced stop should be near-instant.
    async fn force_stop(&self, inst: &Instance) -> anyhow::Result<()> {
        let pid = read_avf_runner_pid(inst).await;
        // RPC may fail (socket gone, runner wedged) — that's fine,
        // we'll signal-escalate via the recorded PID below.
        let rpc_result = avf_rpc(&inst.avf_control_socket_path(), "force_stop").await;

        if let Some(pid) = pid {
            if rpc_result.is_ok() {
                // Give the runner a brief window to tear down via the
                // RPC path before resorting to a signal.
                if wait_for_pid_exit(pid, Duration::from_secs(5)).await.is_ok() {
                    let _ = tokio::fs::remove_file(inst.avf_runner_pid_path()).await;
                    return Ok(());
                }
            }
            avf_terminate_runner(pid).await?;
        } else if let Err(e) = rpc_result {
            // No PID and the RPC failed — nothing left to do.
            return Err(e).context("force_stop: RPC failed and no PID file to fall back on");
        }
        let _ = tokio::fs::remove_file(inst.avf_runner_pid_path()).await;
        Ok(())
    }

    /// Send `{"op":"suspend"}`, wait for the runner to pause the VM,
    /// write its machine state to `<instance>/avf-snapshot.bin`, and
    /// exit cleanly. `agv resume` (i.e. start with `loadvm = Some`)
    /// reads that file back.
    ///
    /// The wait window is wider than `stop`'s: saving full machine
    /// state means writing ~RAM-size bytes to disk. A 4 GiB VM on
    /// internal SSD is roughly 4 s; we allow 60 s so a slow disk or
    /// a large VM still completes. If the runner overruns we don't
    /// SIGTERM — that could corrupt the partial snapshot. Surface
    /// the timeout to the caller instead so they can investigate.
    async fn suspend(&self, inst: &Instance) -> anyhow::Result<()> {
        let pid = read_avf_runner_pid(inst).await;
        avf_rpc(&inst.avf_control_socket_path(), "suspend").await?;

        let Some(pid) = pid else {
            // No PID file but the RPC succeeded — weird state but
            // we accept it. The runner will exit on its own.
            return Ok(());
        };
        wait_for_pid_exit(pid, Duration::from_secs(60))
            .await
            .context(
                "agv-avf-runner did not exit within 60s after suspend RPC \
                 — snapshot write may still be in progress, do not SIGTERM",
            )?;
        // Sanity-check the snapshot landed. Without it, the next
        // `agv resume` would fail with a clearer error than a
        // missing-file open mid-restore.
        let snap = inst.avf_snapshot_path();
        anyhow::ensure!(
            snap.exists(),
            "agv-avf-runner exited but snapshot file {} was not created — \
             check {} for runner errors",
            snap.display(),
            inst.dir.join("avf-runner.log").display(),
        );
        let _ = tokio::fs::remove_file(inst.avf_runner_pid_path()).await;
        Ok(())
    }

    /// Query the runner for the guest's NAT IP. Returns
    /// `(guest_ip, 22)` — AVF's NAT bridge is a real interface on
    /// the host, so SSH reaches the guest directly without the
    /// `hostfwd` plumbing QEMU needs.
    ///
    /// Returns an error if the runner isn't reachable (VM not
    /// running) or if DHCP hasn't completed yet (no `guest_ip` in
    /// the response).
    async fn ssh_endpoint(&self, inst: &Instance) -> anyhow::Result<(String, u16)> {
        let resp = avf_rpc(&inst.avf_control_socket_path(), "status").await?;
        let ip = resp.guest_ip.ok_or_else(|| {
            anyhow::anyhow!(
                "AVF runner has no guest IP yet (DHCP may not have completed)"
            )
        })?;
        Ok((ip, 22))
    }
}

// ---------------------------------------------------------------------------
// AVF runner spawn helpers.
// ---------------------------------------------------------------------------

/// Mirror of the Swift `RunnerConfig` struct (`snake_case` matches
/// the JSON on the wire). Serializing this and writing it to
/// `<instance>/avf-runner-config.json` is what the runner reads on
/// `--config <path>`.
#[cfg(target_os = "macos")]
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
struct AvfRunnerConfig {
    /// Wire-protocol version. The runner refuses to boot on
    /// mismatch — fail-fast for binary-skew installs (e.g.
    /// `cargo install agv` upgraded the Rust side but the user's
    /// `agv-avf-runner` is still from an older tarball). Always
    /// set to [`RUNNER_PROTOCOL_VERSION`] at serialize time.
    runner_protocol_version: u32,
    name: String,
    memory_bytes: u64,
    cpu_count: u32,
    disk_path: String,
    seed_iso_path: String,
    efi_variable_store_path: String,
    serial_log_path: String,
    control_socket_path: String,
    /// Where the runner writes the machine state on suspend, and
    /// where it reads it from on resume. Same file in both directions.
    snapshot_path: String,
    /// True for `agv resume`: runner calls `restoreMachineStateFrom`
    /// and `vm.resume` instead of `vm.start`. False for a cold boot.
    /// The Swift side decodes a missing key as false, but we
    /// always serialize the bool explicitly.
    restore_on_boot: bool,
    /// Sidecar file persisting the VM's MAC address across runner
    /// spawns. Required so `restoreMachineStateFrom` accepts the
    /// new VM config on resume.
    mac_address_path: String,
    /// Sidecar file persisting the VM's `VZGenericMachineIdentifier`
    /// across runner spawns. Same reason as `mac_address_path`.
    machine_identifier_path: String,
}

/// Resolve the agv-avf-runner binary path.
///
/// Lookup order:
///   1. `AGV_AVF_RUNNER` env var (absolute path; intended for dev,
///      pointing at `swift/avf-runner/.build/release/agv-avf-runner`).
///   2. Sibling of the running agv binary (production install: a
///      tarball drops both binaries into the same directory).
///
/// Errors with a clear message if neither exists; the user-facing
/// fix is "run `just build-avf-runner`" or reinstall.
#[cfg(target_os = "macos")]
pub fn locate_avf_runner() -> anyhow::Result<PathBuf> {
    locate_avf_runner_with(
        std::env::var("AGV_AVF_RUNNER").ok(),
        &std::env::current_exe().context("locating current agv binary")?,
    )
}

/// Pure version of [`locate_avf_runner`] — takes the env override and
/// the current-exe path as inputs so it's testable without poking
/// `std::env`.
#[cfg(target_os = "macos")]
fn locate_avf_runner_with(
    env_override: Option<String>,
    current_exe: &Path,
) -> anyhow::Result<PathBuf> {
    if let Some(raw) = env_override {
        let path = PathBuf::from(&raw);
        if path.is_file() {
            return Ok(path);
        }
        bail!("AGV_AVF_RUNNER points at {raw} but no file exists there");
    }
    let parent = current_exe.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "current agv binary has no parent dir: {}",
            current_exe.display()
        )
    })?;
    let candidate = parent.join("agv-avf-runner");
    if candidate.is_file() {
        return Ok(candidate);
    }
    bail!(
        "agv-avf-runner not found next to {} or in $AGV_AVF_RUNNER. \
         Run `just build-avf-runner` and either set AGV_AVF_RUNNER to the \
         build output, or install agv-avf-runner alongside agv.",
        current_exe.display()
    )
}

/// Parse a config memory string (e.g. `"8G"`, `"512M"`) into a byte
/// count for AVF's `memorySize` field. Reuses the same parser the
/// rest of agv uses for disk-size strings.
#[cfg(target_os = "macos")]
fn parse_memory(spec: &str) -> anyhow::Result<u64> {
    crate::image::parse_disk_size(spec)
        .with_context(|| format!("parsing memory spec {spec:?}"))
}

/// Format a runner-failure message that includes the log content
/// inline if present. The runner is detached and its stderr/stdout
/// go to `<inst>/avf-runner.log`; tail of that file is what the
/// user actually needs to debug a startup failure (codesign
/// rejection, `validate()` errors, etc.). Read synchronously — we're
/// already on the error path and the log is small (a few KB at
/// most). On empty/missing log, fall back to just the file path so
/// the user can investigate manually (typical when the runner was
/// `SIGKILL`'d by `AppleSystemPolicy` before producing any output).
#[cfg(target_os = "macos")]
fn runner_failure_message(prefix: &str, log_path: &std::path::Path) -> String {
    let log = std::fs::read_to_string(log_path).unwrap_or_default();
    let log_trimmed = log.trim();
    if log_trimmed.is_empty() {
        format!(
            "{prefix} — runner produced no output. \
             Check {} (empty here, but worth a look in case the kernel \
             SIGKILL'd the runner for a codesign/provenance issue: \
             `log show --predicate 'eventMessage CONTAINS \"agv-avf-runner\"' --last 5m`)",
            log_path.display(),
        )
    } else {
        // Cap at ~2 KiB so we don't dump megabytes if something
        // went terribly wrong. The relevant Swift error is always
        // a single line near the end.
        let tail = if log_trimmed.len() > 2048 {
            let start = log_trimmed.len() - 2048;
            // Don't cut a UTF-8 sequence mid-byte.
            let safe_start = log_trimmed
                .char_indices()
                .find(|(i, _)| *i >= start)
                .map_or(log_trimmed.len(), |(i, _)| i);
            format!("...{}", &log_trimmed[safe_start..])
        } else {
            log_trimmed.to_string()
        };
        format!(
            "{prefix} (see {} for full log):\n  {tail}",
            log_path.display(),
        )
    }
}

/// Read the recorded runner PID, if any. Returns `None` when the
/// file is missing or unparseable — callers treat that as "no
/// runner to clean up" rather than an error, because this is the
/// expected state for a VM that's already stopped.
#[cfg(target_os = "macos")]
async fn read_avf_runner_pid(inst: &Instance) -> Option<u32> {
    let path = inst.avf_runner_pid_path();
    let content = tokio::fs::read_to_string(&path).await.ok()?;
    content.trim().parse::<u32>().ok()
}

/// True iff the PID corresponds to a process that is still
/// executing (not a zombie). Tries `waitpid(WNOHANG)` first to
/// reap and detect exited children we're the parent of; falls
/// back to `is_alive` (signal 0) for non-child PIDs. See
/// [`wait_for_pid_exit`] for context.
#[cfg(target_os = "macos")]
fn pid_is_running(pid: u32) -> bool {
    use rustix::process::{WaitOptions, waitpid};
    let Some(p) = crate::forward::pid_from_u32(pid) else {
        return false;
    };
    match waitpid(Some(p), WaitOptions::NOHANG) {
        // The kernel had an exited child for this PID, and we
        // reaped it. Definitely not running.
        Ok(Some(_status)) => false,
        // Either the child is still running, or this PID is not
        // our child (ECHILD surfaces as Err here on macOS).
        // Either way, fall through to signal-0 to disambiguate.
        Ok(None) | Err(_) => crate::forward::is_alive(pid),
    }
}

/// Poll until the PID disappears or the timeout fires. Returns
/// `Err` only on timeout — caller decides whether to escalate to
/// SIGTERM.
///
/// `is_alive` (signal 0) reports zombies as "alive" — on macOS a
/// zombie process still satisfies `kill(pid, 0)`. The runner is
/// spawned via `Command::spawn` + `mem::forget` (the same
/// detached-process pattern forward supervisors use), so its
/// reaper is whatever process becomes its parent. In production
/// that's `init` (the agv binary exits shortly after sending
/// suspend), but inside a long-lived test binary the runner
/// stays a zombie of the test process until reaped — and `is_alive`
/// would report it alive forever. We complement `is_alive` with a
/// best-effort `waitpid(WNOHANG)` to reap if we happen to be the
/// parent. Both checks return cheaply for non-children (rustix
/// surfaces `ECHILD`), so it's safe to call in either context.
#[cfg(target_os = "macos")]
async fn wait_for_pid_exit(pid: u32, timeout: Duration) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if !pid_is_running(pid) {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            bail!("PID {pid} still alive after {timeout:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Signal the runner's process group. Same primitive
/// `forward::kill_supervisor` uses for forward supervisors — kills
/// the runner and any in-flight subprocess (it doesn't have any
/// today, but defensively it's the right move).
#[cfg(target_os = "macos")]
fn avf_signal_runner(pid: u32, signal: rustix::process::Signal) {
    if let Some(p) = crate::forward::pid_from_u32(pid) {
        let _ = rustix::process::kill_process(p, signal);
    }
}

/// SIGTERM then, if needed, SIGKILL the runner's process group.
/// Returns Ok once the process is gone. Always succeeds eventually
/// unless the kernel itself is unresponsive — SIGKILL can't be caught
/// or blocked, which is the recovery path when the runner is wedged
/// (e.g. the guest issued `sudo halt`, leaving ACPI shutdown unable
/// to resolve, and the runner's signal handler retries the same
/// hopeless ACPI request).
#[cfg(target_os = "macos")]
async fn avf_terminate_runner(pid: u32) -> anyhow::Result<()> {
    avf_signal_runner(pid, rustix::process::Signal::TERM);
    if wait_for_pid_exit(pid, Duration::from_secs(10)).await.is_ok() {
        return Ok(());
    }
    warn!(
        pid,
        "agv-avf-runner did not exit after SIGTERM; escalating to SIGKILL"
    );
    avf_signal_runner(pid, rustix::process::Signal::KILL);
    wait_for_pid_exit(pid, Duration::from_secs(5))
        .await
        .with_context(|| {
            format!("agv-avf-runner (pid {pid}) survived SIGKILL — kernel-level wedge")
        })
}

/// Wait for the runner's control socket to appear. Aborts early if
/// the runner process dies before binding.
#[cfg(target_os = "macos")]
async fn wait_for_avf_socket(
    inst: &Instance,
    pid: u32,
    timeout: Duration,
) -> anyhow::Result<()> {
    let socket_path = inst.avf_control_socket_path();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if socket_path.exists() {
            return Ok(());
        }
        if !crate::forward::is_alive(pid) {
            bail!(
                "agv-avf-runner exited before binding control socket {}",
                socket_path.display()
            );
        }
        if std::time::Instant::now() >= deadline {
            bail!(
                "agv-avf-runner did not bind control socket {} within {timeout:?}",
                socket_path.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Poll the runner's `status` op until `state == "running"` or the
/// timeout fires. Returns Err if the runner exits during the wait.
#[cfg(target_os = "macos")]
async fn wait_for_avf_running(inst: &Instance, timeout: Duration) -> anyhow::Result<()> {
    let socket_path = inst.avf_control_socket_path();
    let deadline = std::time::Instant::now() + timeout;
    let mut last_state: Option<String> = None;
    loop {
        match avf_rpc(&socket_path, "status").await {
            Ok(resp) => {
                if resp.state.as_deref() == Some("running") {
                    return Ok(());
                }
                last_state = resp.state;
            }
            Err(e) => {
                // Socket connect failures are expected briefly during
                // teardown; tolerate a few before surfacing.
                warn!(error = %format!("{e:#}"), "avf status query failed");
            }
        }
        if std::time::Instant::now() >= deadline {
            bail!(
                "agv-avf-runner did not reach `running` within {timeout:?} (last state: {:?})",
                last_state.unwrap_or_else(|| "unknown".to_string())
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC client for the AVF runner's unix-socket control protocol.
// Wire format mirrors swift/avf-runner/Sources/avf-runner/main.swift —
// line-delimited JSON, one request per connection, one response, close.
// ---------------------------------------------------------------------------

/// Decoded shape of a `ControlResponse` from the runner. Mirror of the
/// Swift struct; see runner main.swift.
#[cfg(target_os = "macos")]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct AvfRpcResponse {
    ok: bool,
    error: Option<String>,
    state: Option<String>,
    guest_ip: Option<String>,
}

#[cfg(target_os = "macos")]
const AVF_RPC_TIMEOUT: Duration = Duration::from_secs(5);

/// Send a single JSON-RPC command to the agv-avf-runner control
/// socket and return the parsed response. Returns `Err` if the socket
/// can't be reached, the request times out, the response isn't valid
/// JSON, or the response has `ok: false`.
#[cfg(target_os = "macos")]
async fn avf_rpc(socket_path: &Path, op: &str) -> anyhow::Result<AvfRpcResponse> {
    let conn = tokio::time::timeout(AVF_RPC_TIMEOUT, UnixStream::connect(socket_path))
        .await
        .with_context(|| {
            format!(
                "timed out connecting to AVF runner socket {}",
                socket_path.display()
            )
        })?
        .with_context(|| {
            format!(
                "failed to connect to AVF runner socket {}",
                socket_path.display()
            )
        })?;

    let (read_half, mut write_half) = conn.into_split();

    let request = format!("{{\"op\":\"{op}\"}}\n");
    tokio::time::timeout(AVF_RPC_TIMEOUT, write_half.write_all(request.as_bytes()))
        .await
        .context("timed out sending request to AVF runner")?
        .context("failed to send request to AVF runner")?;
    write_half
        .flush()
        .await
        .context("failed to flush AVF runner request")?;
    drop(write_half);

    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    tokio::time::timeout(AVF_RPC_TIMEOUT, reader.read_line(&mut line))
        .await
        .context("timed out reading response from AVF runner")?
        .context("failed to read response from AVF runner")?;

    let resp: AvfRpcResponse = serde_json::from_str(line.trim())
        .with_context(|| format!("AVF runner returned malformed JSON: {line:?}"))?;
    if !resp.ok {
        bail!(
            "AVF runner returned error for op '{op}': {}",
            resp.error.as_deref().unwrap_or("(no message)")
        );
    }
    Ok(resp)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip a config containing `backend = "qemu"` through
    /// `load_resolved` + `for_config`. Sanity-checks that the
    /// dispatch path picks up the field correctly on every platform.
    #[test]
    fn for_config_dispatches_qemu_on_every_platform() {
        // Construct ResolvedConfig directly rather than going through
        // TOML to keep the test focused on the dispatch shape.
        let cfg = ResolvedConfig {
            backend: "qemu".to_string(),
            ..test_resolved_config()
        };
        // Dispatch returns *something* — we can't compare trait objects
        // directly, but the type-check that for_config(...) returns
        // `&dyn VmBackend` is meaningful in itself.
        let _: &dyn VmBackend = for_config(&cfg);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn for_config_dispatches_avf_on_macos() {
        let cfg = ResolvedConfig {
            backend: "avf".to_string(),
            ..test_resolved_config()
        };
        let _: &dyn VmBackend = for_config(&cfg);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn avf_rpc_round_trip_against_mock_server() {
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("control.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        // Mock server: accept one connection, read a line, send a
        // canned response, close.
        let server_socket = socket_path.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.split();
            let mut reader = tokio::io::BufReader::new(read);
            let mut req = String::new();
            reader.read_line(&mut req).await.unwrap();
            // Sanity: incoming line is the JSON-RPC request.
            assert!(req.contains("\"op\":\"status\""), "got: {req}");
            write
                .write_all(
                    b"{\"ok\":true,\"state\":\"running\",\"guest_ip\":\"192.168.64.5\"}\n",
                )
                .await
                .unwrap();
            write.shutdown().await.ok();
            // Hold the listener until response is on the wire.
            let _ = server_socket;
        });

        let resp = avf_rpc(&socket_path, "status").await.unwrap();
        assert!(resp.ok);
        assert_eq!(resp.guest_ip.as_deref(), Some("192.168.64.5"));
        server.await.unwrap();
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn avf_rpc_propagates_runner_error() {
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("control.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.split();
            let mut reader = tokio::io::BufReader::new(read);
            let mut req = String::new();
            reader.read_line(&mut req).await.unwrap();
            write
                .write_all(b"{\"ok\":false,\"error\":\"unknown op 'bogus'\"}\n")
                .await
                .unwrap();
            write.shutdown().await.ok();
        });

        let err = avf_rpc(&socket_path, "bogus").await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unknown op 'bogus'"),
            "expected runner error in message, got: {msg}"
        );
        server.await.unwrap();
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn avf_rpc_fails_when_socket_missing() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("nonexistent.sock");
        let err = avf_rpc(&socket_path, "status").await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("failed to connect") || msg.contains("timed out connecting"),
            "expected connect-failure message, got: {msg}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn locate_avf_runner_honors_env_var() {
        // Use a known-existing file (the agv test binary itself) as
        // a placeholder — we're testing path-resolution, not the
        // returned binary's behavior.
        let exe = std::env::current_exe().unwrap();
        let resolved =
            locate_avf_runner_with(Some(exe.display().to_string()), &exe).unwrap();
        assert_eq!(resolved, exe);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn locate_avf_runner_rejects_bogus_env_var() {
        let exe = std::env::current_exe().unwrap();
        let err = locate_avf_runner_with(
            Some("/no/such/path/agv-avf-runner".to_string()),
            &exe,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("AGV_AVF_RUNNER points at"),
            "expected env-var-not-found message, got: {msg}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn locate_avf_runner_finds_sibling_of_current_exe() {
        // Drop a fake binary next to the current exe and confirm
        // resolver picks it up when no env override is given.
        let dir = tempfile::tempdir().unwrap();
        let fake_exe = dir.path().join("agv");
        std::fs::write(&fake_exe, b"#!/bin/sh\nexit 0\n").unwrap();
        let runner = dir.path().join("agv-avf-runner");
        std::fs::write(&runner, b"#!/bin/sh\nexit 0\n").unwrap();
        let resolved = locate_avf_runner_with(None, &fake_exe).unwrap();
        assert_eq!(resolved, runner);
    }

    /// `wait_for_pid_exit` returns Ok the moment a target process
    /// exits. Spawn a `sleep` child, observe its PID is alive, then
    /// kill it and confirm the helper sees the exit.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn wait_for_pid_exit_returns_once_pid_dies() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawning sleep");
        let pid = child.id();
        // Brief sanity: the child is alive right now.
        assert!(crate::forward::is_alive(pid));
        // Kick off the watcher *first*, then kill — so the watcher
        // observes a live-then-dead transition (not a pid that was
        // already gone).
        let waiter = tokio::spawn(async move {
            wait_for_pid_exit(pid, Duration::from_secs(5)).await
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        child.kill().expect("kill sleep");
        child.wait().expect("reap sleep");
        let result = waiter.await.unwrap();
        assert!(result.is_ok(), "expected Ok after pid exit, got {result:?}");
    }

    /// `wait_for_pid_exit` reports timeout when the target keeps
    /// running. Uses a 200ms ceiling against a long-lived `sleep`
    /// so the test is quick.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn wait_for_pid_exit_times_out_when_pid_lives() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawning sleep");
        let pid = child.id();
        let result = wait_for_pid_exit(pid, Duration::from_millis(200)).await;
        let err = result.expect_err("expected timeout error");
        assert!(
            format!("{err:#}").contains("still alive"),
            "expected timeout message, got: {err:#}"
        );
        child.kill().expect("kill sleep");
        child.wait().expect("reap sleep");
    }

    /// `avf_terminate_runner` escalates SIGTERM → SIGKILL when the
    /// target ignores SIGTERM. Without this, an AVF runner wedged on
    /// an ACPI shutdown that never resolves (e.g. guest issued
    /// `sudo halt`) would leave `agv stop` unable to recover.
    /// Reproduce with a bash child that `trap`s SIGTERM and ignores
    /// it; only SIGKILL gets through.
    ///
    /// Timing assertion: the SIGTERM window in `avf_terminate_runner`
    /// is 10s, so a successful escalation must take *at least* that
    /// long. If the test returns in <10s we know SIGTERM accidentally
    /// killed the child (test is invalid) rather than the escalation
    /// firing as designed.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn avf_terminate_runner_escalates_to_sigkill_when_sigterm_ignored() {
        // Use Perl rather than a shell `trap`: bash's `trap '' TERM`
        // is unreliable here because bash forks an external `sleep`
        // and the wait/signal interaction can fall through (verified
        // by an earlier timing-assert run that exited in 100ms).
        // Perl's `$SIG{TERM} = "IGNORE"` sets the kernel-level
        // disposition to SIG_IGN in the same process that calls
        // `sleep`, which is the rigorous reproduction we need.
        // macOS ships Perl by default.
        let mut child = std::process::Command::new("perl")
            .args(["-e", "$SIG{TERM} = \"IGNORE\"; sleep 60"])
            .spawn()
            .expect("spawning sigterm-ignoring perl");
        let pid = child.id();
        // Give perl a moment to install the signal handler. Without
        // this delay, an early SIGTERM lands before `$SIG{TERM} =
        // "IGNORE"` executes — kernel default disposition (terminate)
        // wins and the test sees a sub-second exit that *looks* like
        // a broken escalation but is really a race.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(crate::forward::is_alive(pid));

        let start = std::time::Instant::now();
        let result = avf_terminate_runner(pid).await;
        let elapsed = start.elapsed();

        assert!(
            result.is_ok(),
            "avf_terminate_runner should succeed via SIGKILL escalation, got: {result:?}"
        );
        assert!(
            !crate::forward::is_alive(pid),
            "process should be gone after avf_terminate_runner returns"
        );
        assert!(
            elapsed >= Duration::from_secs(10),
            "escalation must wait the full 10s SIGTERM window before SIGKILL — \
             got {elapsed:?}, suggesting SIGTERM killed the child unexpectedly \
             and the test isn't actually exercising the escalation path"
        );
        let _ = child.wait();
    }

    /// `read_avf_runner_pid` is forgiving — missing or malformed
    /// content returns None rather than erroring. Callers rely on
    /// that to skip cleanup when no runner exists.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn read_avf_runner_pid_handles_missing_and_malformed() {
        let dir = tempfile::tempdir().unwrap();
        let inst = Instance {
            name: "tmp".to_string(),
            dir: dir.path().to_path_buf(),
        };

        // No file → None.
        assert!(read_avf_runner_pid(&inst).await.is_none());

        // Malformed content → None.
        tokio::fs::write(inst.avf_runner_pid_path(), b"not-a-number")
            .await
            .unwrap();
        assert!(read_avf_runner_pid(&inst).await.is_none());

        // Well-formed content → Some.
        tokio::fs::write(inst.avf_runner_pid_path(), b"42\n")
            .await
            .unwrap();
        assert_eq!(read_avf_runner_pid(&inst).await, Some(42));
    }

    /// Build a minimal valid `ResolvedConfig` for tests in this module.
    /// Mirrors the template-clone shape from `vm/template.rs`.
    fn test_resolved_config() -> ResolvedConfig {
        ResolvedConfig {
            base_url: String::new(),
            base_checksum: String::new(),
            skip_checksum: true,
            memory: "1G".to_string(),
            cpus: 1,
            disk: "10G".to_string(),
            user: "agent".to_string(),
            os_family: "debian".to_string(),
            files: vec![],
            setup: vec![],
            provision: vec![],
            forwards: vec![],
            auto_forwards: std::collections::BTreeMap::new(),
            template_name: None,
            mixins_applied: vec![],
            mixin_notes: vec![],
            config_notes: vec![],
            mixin_manual_steps: vec![],
            config_manual_steps: vec![],
            labels: std::collections::BTreeMap::new(),
            idle_suspend_minutes: 0,
            idle_load_threshold: 0.2,
            machine_type: None,
            backend: "qemu".to_string(),
        }
    }
}
