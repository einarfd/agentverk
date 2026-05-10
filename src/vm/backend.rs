//! Pluggable VM execution backend.
//!
//! The trait isolates the lifecycle calls that differ between hypervisors —
//! today only QEMU is implemented; Apple Virtualization (AVF) on macOS will
//! land as a second impl. Everything above the boundary (cloud-init, SSH,
//! mixins, port forwards, idle watcher) stays backend-agnostic and uses
//! the trait through `&dyn VmBackend`.
//!
//! This file is the foundation commit: trait definition plus a
//! `LocalQemuBackend` that delegates to the existing `vm::qemu` module.
//! Lifecycle call sites still use `vm::qemu::*` directly — they'll be
//! migrated to the backend in follow-up commits so each step is small
//! and reviewable.

use async_trait::async_trait;

use crate::config::ResolvedConfig;
use crate::vm::instance::Instance;
use crate::vm::qemu;

/// Backends own VM lifecycle: boot, stop, suspend/resume, and the SSH
/// endpoint of the guest.
///
/// Methods are designed so the caller doesn't need to know whether QEMU's
/// `hostfwd` model or AVF's NAT-IP model is in use — `ssh_endpoint`
/// abstracts the difference. `start` takes the resolved config plus a
/// `machine_type` (only QEMU uses it; AVF will ignore the parameter).
#[async_trait]
pub trait VmBackend: Send + Sync {
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

/// Singleton backend instance.
static LOCAL_QEMU: LocalQemuBackend = LocalQemuBackend;

/// Pick the backend for a VM by inspecting its resolved config.
///
/// Today only `"qemu"` is implemented; `LocalAvfBackend` will plug in
/// here when it lands. The config field is validated at load time
/// ([`crate::config::load_resolved`]) so this function is infallible.
#[must_use]
pub fn for_config(cfg: &ResolvedConfig) -> &'static dyn VmBackend {
    match cfg.backend.as_str() {
        "qemu" => &LOCAL_QEMU,
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
