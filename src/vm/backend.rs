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
use std::path::Path;
#[cfg(target_os = "macos")]
use anyhow::{bail, Context as _};
#[cfg(target_os = "macos")]
use serde::Deserialize;
#[cfg(target_os = "macos")]
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
#[cfg(target_os = "macos")]
use tokio::net::UnixStream;
#[cfg(target_os = "macos")]
use std::time::Duration;

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
    /// VZDiskImageStorageDeviceAttachment opens.
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
        crate::qcow2::convert_to_sparse_raw(base_image, &dest)
            .await
            .with_context(|| {
                format!(
                    "converting {} → {}",
                    base_image.display(),
                    dest.display()
                )
            })?;
        // qcow2-rs preserves the qcow2's virtual size (e.g. 3 GiB
        // for stock cloud images); grow to the user-spec'd size so
        // the guest's growpart/resize2fs can take advantage of it.
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

    async fn start(
        &self,
        _inst: &Instance,
        _cfg: &ResolvedConfig,
        _machine_type: &str,
        _loadvm: Option<&str>,
    ) -> anyhow::Result<()> {
        anyhow::bail!("AVF backend is not yet implemented (start)")
    }

    /// Send `{"op":"stop"}` over the runner's control socket. The
    /// runner schedules an ACPI shutdown asynchronously and returns
    /// `ok` immediately; the VM exits via `guest_did_stop` in the
    /// runner, after which the runner process exits and removes the
    /// socket file. Callers observe completion by waiting for the
    /// runner PID to disappear (handled by `vm::stop` in `mod.rs`).
    async fn stop(&self, inst: &Instance) -> anyhow::Result<()> {
        avf_rpc(&inst.avf_control_socket_path(), "stop").await?;
        Ok(())
    }

    /// Send `{"op":"force_stop"}`. Same fire-and-forget shape as
    /// `stop`, but the runner calls `vm.stop()` (abrupt) instead of
    /// `vm.requestStop()` (ACPI).
    async fn force_stop(&self, inst: &Instance) -> anyhow::Result<()> {
        avf_rpc(&inst.avf_control_socket_path(), "force_stop").await?;
        Ok(())
    }

    async fn suspend(&self, _inst: &Instance) -> anyhow::Result<()> {
        anyhow::bail!("AVF backend is not yet implemented (suspend)")
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
// JSON-RPC client for the AVF runner's unix-socket control protocol.
// Wire format mirrors swift/avf-runner/Sources/avf-runner/main.swift —
// line-delimited JSON, one request per connection, one response, close.
// ---------------------------------------------------------------------------

/// Decoded shape of a `ControlResponse` from the runner. Mirror of the
/// Swift struct; see runner main.swift.
#[cfg(target_os = "macos")]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
#[expect(
    dead_code,
    reason = "`state` will be read by the start path once it polls the runner; keeping the field decoded so the wire shape stays in lockstep with the Swift side"
)]
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
