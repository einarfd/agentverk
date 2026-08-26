//! QEMU process spawning and QMP protocol communication.
//!
//! Handles starting QEMU as a background process, communicating over the
//! QMP JSON socket for lifecycle management, and graceful/forceful shutdown.

use std::path::Path;
#[cfg(target_arch = "aarch64")]
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context as _};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{debug, info, warn};

use crate::vm::instance::Instance;

/// Spawn a QEMU process for the given VM instance.
///
/// Allocates a free port for SSH forwarding, builds the QEMU command line,
/// and spawns QEMU as a detached child. On success, the PID file, the SSH
/// port file, and the QMP socket all exist — this returns only once QEMU
/// has created the socket.
///
/// `machine_type` is the pinned `-machine` value (e.g. `pc-q35-9.2`,
/// `virt-9.2`). The caller is expected to resolve this once via
/// [`current_default_machine_type`] when not yet pinned, persist it, and
/// pass the same value on every subsequent start so QEMU upgrades don't
/// silently change the guest's device topology.
pub async fn start(
    instance: &Instance,
    memory: &str,
    cpus: u32,
    machine_type: &str,
) -> anyhow::Result<()> {
    start_with_loadvm(instance, memory, cpus, machine_type, None).await
}

/// Start QEMU, optionally loading from a saved snapshot.
pub async fn start_with_loadvm(
    instance: &Instance,
    memory: &str,
    cpus: u32,
    machine_type: &str,
    loadvm: Option<&str>,
) -> anyhow::Result<()> {
    let ssh_port = allocate_free_port().await?;
    let (binary, mut args) = build_qemu_args(instance, memory, cpus, ssh_port, machine_type)?;
    if let Some(snapshot) = loadvm {
        args.push("-loadvm".to_string());
        args.push(snapshot.to_string());
    }

    info!(
        vm = %instance.name,
        binary = %binary,
        ssh_port,
        "starting QEMU"
    );

    let pid = spawn_detached(instance, &binary, &args)?;

    // `-daemonize` used to provide the "started successfully" signal:
    // its parent process only exited once the child had finished
    // setting up, so a zero exit status meant QEMU was up.
    if let Err(failure) = wait_for_qmp_ready(instance, pid, QMP_READY_TIMEOUT).await {
        // Only signal a QEMU known to be alive. On the `Exited` path the
        // reaper in `spawn_detached` has already collected the process,
        // so the PID may belong to something else entirely by now.
        if matches!(failure, StartupFailure::Stalled(_)) {
            force_kill(pid);
        }
        let _ = tokio::fs::remove_file(instance.pid_path()).await;
        let _ = tokio::fs::remove_file(instance.qmp_socket_path()).await;
        bail!(
            "{}",
            qemu_failure_message(failure.reason(), &instance.qemu_log_path())
        );
    }

    // Write the SSH port so other commands (ssh, scp) can find it.
    tokio::fs::write(instance.ssh_port_path(), ssh_port.to_string())
        .await
        .context("failed to write SSH port file")?;

    info!(vm = %instance.name, "QEMU started");
    Ok(())
}

/// Send a graceful shutdown command via the QMP socket.
///
/// Connects to the QMP socket and sends `system_powerdown` (ACPI power button).
/// Waits up to 30 seconds for the process to exit, then falls back to `force_stop`.
pub async fn stop(instance: &Instance) -> anyhow::Result<()> {
    let socket_path = instance.qmp_socket_path();
    info!(vm = %instance.name, "sending graceful shutdown via QMP");

    // Read the pid before asking QEMU to exit: the poll loop below
    // needs it, and once QEMU is gone `cleanup_runtime_files` removes
    // the file. agv writes this file itself now (QEMU is spawned
    // without `-pidfile`), so it outlives the process rather than
    // being unlinked from under us mid-shutdown.
    let pid = read_pid(instance).await?;

    let mut client = QmpClient::connect(&socket_path).await?;
    client.execute("system_powerdown").await?;

    // Poll for process exit, up to 30 seconds.
    for i in 0..60 {
        if !is_process_alive(pid) {
            debug!(vm = %instance.name, elapsed_secs = i / 2, "QEMU exited gracefully");
            cleanup_runtime_files(instance).await;
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    warn!(vm = %instance.name, "graceful shutdown timed out after 30s, force-killing");
    force_stop(instance).await
}

/// Suspend the VM by saving its full state to a snapshot in the qcow2 disk,
/// then exiting QEMU.
///
/// Uses the HMP `savevm` command via QMP `human-monitor-command`. The
/// snapshot name is fixed (`agv-suspend`) since each VM has at most one
/// suspended state.
pub async fn suspend(instance: &Instance) -> anyhow::Result<()> {
    let socket_path = instance.qmp_socket_path();
    info!(vm = %instance.name, "saving VM state via QMP savevm");

    // Read the pid up front — it is needed both to poll for exit below
    // and to tell "QEMU rejected the command" apart from "QEMU died"
    // if the save fails.
    let pid = read_pid(instance).await?;

    let mut client = QmpClient::connect(&socket_path).await?;
    // Run `savevm agv-suspend` via the human monitor.
    if let Err(e) = client.execute_hmp("savevm agv-suspend").await {
        // A savevm that takes QEMU down with it surfaces here only as an
        // EOF on the monitor socket, which on its own says nothing about
        // why. Give the exit a moment to land before concluding QEMU is
        // alive — the kernel closes its fds (producing this EOF) before
        // the process is reaped, so an immediate check reports "alive"
        // exactly in the case the log excerpt was captured for.
        if wait_for_exit(pid, QEMU_EXIT_GRACE).await {
            bail!(
                "{}",
                qemu_failure_message(
                    &format!("QEMU exited during savevm: {e:#}"),
                    &instance.qemu_log_path(),
                )
            );
        }
        return Err(e.context("failed to save VM state"));
    }
    info!(vm = %instance.name, "VM state saved, shutting down QEMU");

    // Quit QEMU cleanly now that the snapshot is on disk.
    // The `quit` command causes QEMU to exit immediately without ACPI shutdown.
    let _ = client.execute("quit").await;
    drop(client);

    // Wait for the process to exit.
    for _ in 0..60 {
        if !is_process_alive(pid) {
            cleanup_runtime_files(instance).await;
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    warn!(vm = %instance.name, "QEMU did not exit after quit, force-killing");
    force_stop(instance).await
}

/// Force-kill the QEMU process using the PID file.
pub async fn force_stop(instance: &Instance) -> anyhow::Result<()> {
    let Ok(pid) = read_pid(instance).await else {
        debug!(vm = %instance.name, "no PID file found — process already gone");
        cleanup_runtime_files(instance).await;
        return Ok(());
    };

    info!(vm = %instance.name, pid, "force-killing QEMU process");
    let _ = kill_process(pid, rustix::process::Signal::KILL);

    // Brief sleep to let the OS clean up.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    cleanup_runtime_files(instance).await;
    Ok(())
}

// ---------------------------------------------------------------------------
// QMP client
// ---------------------------------------------------------------------------

/// Minimal QMP (QEMU Machine Protocol) client.
///
/// Communicates over a Unix socket using JSON-line protocol. Handles the
/// initial handshake (greeting + `qmp_capabilities`) and command execution.
struct QmpClient {
    reader: BufReader<tokio::io::ReadHalf<UnixStream>>,
    writer: tokio::io::WriteHalf<UnixStream>,
}

impl QmpClient {
    /// Connect to a QMP socket and perform the initial handshake.
    async fn connect(socket_path: &Path) -> anyhow::Result<Self> {
        let path_str = socket_path
            .to_str()
            .context("QMP socket path is not valid UTF-8")?;

        let stream = UnixStream::connect(socket_path)
            .await
            .with_context(|| format!("failed to connect to QMP socket at {path_str}"))?;

        let (read_half, write_half) = tokio::io::split(stream);
        let mut client = Self {
            reader: BufReader::new(read_half),
            writer: write_half,
        };

        // Read the QMP greeting.
        let greeting = client.read_response().await?;
        if greeting.get("QMP").is_none() {
            bail!("unexpected QMP greeting: {greeting}");
        }

        // Send qmp_capabilities to enter command mode.
        client
            .send_raw(r#"{"execute":"qmp_capabilities"}"#)
            .await?;
        let resp = client.read_response().await?;
        if resp.get("return").is_none() {
            bail!("QMP qmp_capabilities failed: {resp}");
        }

        Ok(client)
    }

    /// Execute a QMP command and return the response.
    async fn execute(&mut self, command: &str) -> anyhow::Result<serde_json::Value> {
        let msg = format!(r#"{{"execute":"{command}"}}"#);
        self.send_raw(&msg).await?;
        let resp = self.read_response().await?;
        if let Some(error) = resp.get("error") {
            bail!("QMP command '{command}' failed: {error}");
        }
        Ok(resp)
    }

    /// Execute a Human Monitor (HMP) command via the QMP human-monitor-command
    /// wrapper. Used for HMP-only operations like `savevm`/`loadvm`.
    async fn execute_hmp(&mut self, command: &str) -> anyhow::Result<serde_json::Value> {
        let escaped = command.replace('\\', "\\\\").replace('"', "\\\"");
        let msg = format!(
            r#"{{"execute":"human-monitor-command","arguments":{{"command-line":"{escaped}"}}}}"#
        );
        self.send_raw(&msg).await?;
        let resp = self.read_response().await?;
        if let Some(error) = resp.get("error") {
            bail!("HMP command '{command}' failed: {error}");
        }
        // The "return" field of human-monitor-command is the HMP output as a
        // string. Most HMP commands are silent on success — treat any
        // non-empty output as an error.
        if let Some(ret) = resp.get("return").and_then(|v| v.as_str())
            && !ret.trim().is_empty()
        {
            bail!("HMP command '{command}' returned error: {}", ret.trim());
        }
        Ok(resp)
    }

    /// Read a single JSON response, skipping asynchronous event messages.
    async fn read_response(&mut self) -> anyhow::Result<serde_json::Value> {
        loop {
            let mut line = String::new();
            let n = self
                .reader
                .read_line(&mut line)
                .await
                .context("failed to read from QMP socket")?;
            if n == 0 {
                bail!("QMP socket closed unexpectedly");
            }
            let value: serde_json::Value =
                serde_json::from_str(line.trim()).context("failed to parse QMP response")?;
            // Skip async events; they have an "event" key.
            if value.get("event").is_some() {
                debug!(event = %value, "skipping QMP event");
                continue;
            }
            return Ok(value);
        }
    }

    /// Send a raw JSON string followed by a newline.
    async fn send_raw(&mut self, msg: &str) -> anyhow::Result<()> {
        self.writer
            .write_all(msg.as_bytes())
            .await
            .context("failed to write to QMP socket")?;
        self.writer
            .write_all(b"\n")
            .await
            .context("failed to write newline to QMP socket")?;
        self.writer
            .flush()
            .await
            .context("failed to flush QMP socket")?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Allocate a free TCP port by binding to port 0 and reading the assignment.
///
/// There is a small TOCTOU window between when we release the port and when
/// QEMU binds to it, but this is acceptable for our use case.
/// Detect a disk image format from the file's magic bytes.
///
/// Returns `"qcow2"` if the first 4 bytes match the qcow2 magic (`QFI\xfb`),
/// otherwise `"raw"`. Used for the EFI vars file, which may be raw on
/// existing VMs created before the qcow2 conversion was added.
#[cfg(target_arch = "aarch64")]
fn detect_image_format(path: &Path) -> anyhow::Result<&'static str> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    let mut magic = [0u8; 4];
    if file.read_exact(&mut magic).is_ok() && &magic == b"QFI\xfb" {
        Ok("qcow2")
    } else {
        Ok("raw")
    }
}

pub(super) async fn allocate_free_port() -> anyhow::Result<u16> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("failed to bind to ephemeral port")?;
    let port = listener
        .local_addr()
        .context("failed to get local address of ephemeral port")?
        .port();
    // Drop the listener to free the port for QEMU.
    drop(listener);
    Ok(port)
}

/// Check whether the host supports nested virtualization for guests.
///
/// On Linux `x86_64`: checks the KVM kernel module `nested` parameter.
/// On Linux aarch64: checks if KVM is available (virt extensions are
///   hardware-level; if KVM works, the CPU supports EL2).
/// On macOS: not called (HVF doesn't support it yet in released QEMU).
#[cfg(target_os = "linux")]
fn nested_virt_available() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        for path in &[
            "/sys/module/kvm_intel/parameters/nested",
            "/sys/module/kvm_amd/parameters/nested",
        ] {
            if let Ok(val) = std::fs::read_to_string(path) {
                let trimmed = val.trim();
                if trimmed == "1" || trimmed == "Y" {
                    return true;
                }
            }
        }
        false
    }

    #[cfg(target_arch = "aarch64")]
    {
        // ARM has no kernel module parameter for nested virt — EL2 support is
        // a hardware capability, not a software toggle. If KVM is running, the
        // CPU has EL2. This check is imperfect: /dev/kvm can exist inside a VM
        // (L1) that doesn't support nesting, which would cause QEMU to fail
        // when we set virtualization=on. The error message is clear in that case.
        Path::new("/dev/kvm").exists()
    }
}

/// Per-platform info needed to build a QEMU command line.
struct PlatformArgs {
    /// Binary name (e.g. `qemu-system-x86_64`, `qemu-system-aarch64`).
    binary: String,
    /// Unversioned machine alias for the platform (`q35` or `virt`).
    /// Used as the resolution target when no pinned `machine_type` is set.
    machine_alias: &'static str,
    /// Comma-suffix segments to append to the resolved/pinned machine type
    /// (e.g. `["virtualization=on"]` for nested arm). Empty on platforms
    /// that need no extras.
    machine_extras: Vec<String>,
    /// Accelerator and CPU flags (e.g. `-accel kvm -cpu host`). Emitted
    /// verbatim after the `-machine` flag.
    accel_and_cpu_args: Vec<String>,
}

/// Return the QEMU binary name and platform-specific machine/accel args.
#[expect(
    clippy::unnecessary_wraps,
    reason = "returns Err on unsupported platforms via #[cfg]; clippy can't see the conditional branches"
)]
fn platform_args() -> anyhow::Result<PlatformArgs> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        Ok(PlatformArgs {
            binary: "qemu-system-aarch64".to_string(),
            machine_alias: "virt",
            machine_extras: vec![],
            accel_and_cpu_args: vec![
                "-accel".to_string(),
                "hvf".to_string(),
                "-cpu".to_string(),
                "host".to_string(),
            ],
        })
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        let nested = nested_virt_available();
        if nested {
            info!("nested virtualization: enabled (host KVM module supports it)");
        }
        Ok(PlatformArgs {
            binary: "qemu-system-x86_64".to_string(),
            machine_alias: "q35",
            machine_extras: vec![],
            accel_and_cpu_args: vec![
                "-accel".to_string(),
                "kvm".to_string(),
                "-cpu".to_string(),
                "host".to_string(),
            ],
        })
    }

    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        let nested = nested_virt_available();
        let mut extras: Vec<String> = vec![];
        if nested {
            info!("nested virtualization: enabled (virtualization=on)");
            extras.push("virtualization=on".to_string());
        }
        Ok(PlatformArgs {
            binary: "qemu-system-aarch64".to_string(),
            machine_alias: "virt",
            machine_extras: extras,
            accel_and_cpu_args: vec![
                "-accel".to_string(),
                "kvm".to_string(),
                "-cpu".to_string(),
                "host".to_string(),
            ],
        })
    }

    #[cfg(not(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
    )))]
    {
        bail!("unsupported platform: agv requires macOS/aarch64, Linux/x86_64, or Linux/aarch64")
    }
}

/// Resolve the host QEMU's current default version of this platform's
/// machine alias (e.g. `q35` → `pc-q35-9.2`). Used for the auto-pin path
/// when an instance has no `machine_type` set yet.
///
/// Shells out to `qemu-system-X -machine help` and parses the listing.
/// Modern QEMU annotates the alias with `(alias of pc-q35-X.Y)`; we use
/// that when present and fall back to "latest `<prefix>-X.Y` line" if
/// not. Returns the pinned name (e.g. `"pc-q35-9.2"`).
pub fn current_default_machine_type() -> anyhow::Result<String> {
    let p = platform_args()?;
    let output = std::process::Command::new(&p.binary)
        .args(["-machine", "help"])
        .output()
        .with_context(|| format!("failed to run `{} -machine help`", p.binary))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "`{} -machine help` failed (exit {}): {stderr}",
            p.binary,
            output.status,
        );
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_machine_help(&text, p.machine_alias).with_context(|| {
        format!(
            "could not resolve a pinned version for `-machine {}` from `{} -machine help` output",
            p.machine_alias, p.binary,
        )
    })
}

/// Parse the output of `qemu-system-X -machine help` to find the pinned
/// version of `alias` (e.g. `q35` → `pc-q35-9.2`).
///
/// Strategy:
/// 1. Look for the alias's own line and read `(alias of pc-q35-X.Y)`. This
///    is what every modern QEMU emits.
/// 2. Fall back to the highest-version `<prefix>-X.Y` line in the listing,
///    where prefix is `pc-q35-` for `q35` and `virt-` for `virt`.
fn parse_machine_help(help_output: &str, alias: &str) -> Option<String> {
    // Strategy 1: look for an `(alias of XXX)` annotation on the alias line.
    for line in help_output.lines() {
        let mut tokens = line.split_ascii_whitespace();
        if tokens.next() != Some(alias) {
            continue;
        }
        if let Some(idx) = line.find("(alias of ") {
            let rest = &line[idx + "(alias of ".len()..];
            if let Some(end) = rest.find(')') {
                let resolved = rest[..end].trim();
                if !resolved.is_empty() {
                    return Some(resolved.to_string());
                }
            }
        }
    }

    // Strategy 2: scan for the highest-version `<prefix>-X.Y` line.
    let prefix = match alias {
        "q35" => "pc-q35-",
        "virt" => "virt-",
        _ => return None,
    };
    let mut best: Option<((u32, u32), String)> = None;
    for line in help_output.lines() {
        let Some(name) = line.split_ascii_whitespace().next() else {
            continue;
        };
        if line.contains("(deprecated)") {
            continue;
        }
        let Some(suffix) = name.strip_prefix(prefix) else {
            continue;
        };
        let mut parts = suffix.splitn(2, '.');
        let Some(major) = parts.next().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Some(minor) = parts.next().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let key = (major, minor);
        if best.as_ref().is_none_or(|(k, _)| key > *k) {
            best = Some((key, name.to_string()));
        }
    }
    best.map(|(_, name)| name)
}

/// EFI firmware paths for aarch64: read-only code and writable vars template.
#[cfg(target_arch = "aarch64")]
struct EfiFirmware {
    code: PathBuf,
    vars: PathBuf,
}

/// Find the EFI firmware files for aarch64 QEMU.
///
/// Returns paths to both the read-only code image and the writable NVRAM
/// vars template. The vars file must be copied to the instance directory
/// before use, since each VM needs its own writable copy.
#[cfg(target_arch = "aarch64")]
fn find_efi_firmware() -> anyhow::Result<EfiFirmware> {
    let (code_candidates, vars_candidates): (&[&str], &[&str]) = if cfg!(target_os = "macos") {
        (
            &[
                "/opt/homebrew/share/qemu/edk2-aarch64-code.fd",
                "/usr/local/share/qemu/edk2-aarch64-code.fd",
            ],
            &[
                "/opt/homebrew/share/qemu/edk2-arm-vars.fd",
                "/usr/local/share/qemu/edk2-arm-vars.fd",
            ],
        )
    } else {
        (
            &[
                "/usr/share/qemu-efi-aarch64/QEMU_EFI.fd",
                "/usr/share/AAVMF/AAVMF_CODE.fd",
                "/usr/share/edk2/aarch64/QEMU_EFI.fd",
                "/usr/share/qemu/edk2-aarch64-code.fd",
            ],
            &[
                "/usr/share/AAVMF/AAVMF_VARS.fd",
                "/usr/share/edk2/aarch64/vars-template-pflash.raw",
                "/usr/share/qemu/edk2-arm-vars.fd",
            ],
        )
    };

    let code = code_candidates
        .iter()
        .map(Path::new)
        .find(|p| p.exists())
        .map(Path::to_path_buf);

    let vars = vars_candidates
        .iter()
        .map(Path::new)
        .find(|p| p.exists())
        .map(Path::to_path_buf);

    match (code, vars) {
        (Some(code), Some(vars)) => Ok(EfiFirmware { code, vars }),
        (None, _) => {
            let searched = code_candidates.join(", ");
            bail!(
                "EFI firmware code not found for aarch64 — searched: {searched}\n\
                 Install QEMU with EFI support:\n\
                 \x20 macOS: brew install qemu\n\
                 \x20 Debian/Ubuntu: apt install qemu-efi-aarch64\n\
                 \x20 Fedora: dnf install edk2-aarch64"
            )
        }
        (_, None) => {
            let searched = vars_candidates.join(", ");
            bail!(
                "EFI NVRAM vars template not found for aarch64 — searched: {searched}\n\
                 Install QEMU with EFI support:\n\
                 \x20 macOS: brew install qemu\n\
                 \x20 Debian/Ubuntu: apt install qemu-efi-aarch64\n\
                 \x20 Fedora: dnf install edk2-aarch64"
            )
        }
    }
}

/// Build the full QEMU argument list.
#[expect(
    clippy::too_many_lines,
    reason = "linear builder for the full QEMU command line; splitting it would just hide the structure"
)]
fn build_qemu_args(
    instance: &Instance,
    memory: &str,
    cpus: u32,
    ssh_port: u16,
    machine_type: &str,
) -> anyhow::Result<(String, Vec<String>)> {
    let p = platform_args()?;
    let binary = p.binary;
    let mut args: Vec<String> = Vec::new();
    let machine_value = if p.machine_extras.is_empty() {
        machine_type.to_string()
    } else {
        format!("{machine_type},{}", p.machine_extras.join(","))
    };
    args.extend(["-machine".to_string(), machine_value]);
    args.extend(p.accel_and_cpu_args);

    // EFI firmware for aarch64: pflash drives for code (read-only) and
    // vars (writable per-instance copy for UEFI NVRAM).
    #[cfg(target_arch = "aarch64")]
    {
        let firmware = find_efi_firmware()?;
        let code_str = firmware
            .code
            .to_str()
            .context("EFI firmware code path is not valid UTF-8")?;
        let vars_dst = instance.efi_vars_path();

        // Create the vars file as qcow2 if it doesn't exist yet. qcow2 is
        // required for `savevm`/`loadvm` (suspend/resume) to work — raw drives
        // do not support snapshots.
        if !vars_dst.exists() {
            let src_str = firmware
                .vars
                .to_str()
                .context("EFI vars template path is not valid UTF-8")?;
            let dst_str = vars_dst
                .to_str()
                .context("EFI vars path is not valid UTF-8")?;
            let output = std::process::Command::new("qemu-img")
                .args(["convert", "-f", "raw", "-O", "qcow2", src_str, dst_str])
                .output()
                .with_context(|| {
                    format!(
                        "failed to run qemu-img convert for EFI vars: {} → {}",
                        firmware.vars.display(),
                        vars_dst.display()
                    )
                })?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                bail!("qemu-img convert failed (exit {}): {stderr}", output.status);
            }
        }

        // Detect the format of the existing file. New VMs always get qcow2,
        // but pre-existing VMs created before this change have raw files.
        let vars_format = detect_image_format(&vars_dst)?;
        let vars_str = vars_dst
            .to_str()
            .context("EFI vars path is not valid UTF-8")?;
        args.extend([
            "-drive".to_string(),
            format!("if=pflash,format=raw,readonly=on,file={code_str}"),
            "-drive".to_string(),
            format!("if=pflash,format={vars_format},file={vars_str}"),
        ]);
    }

    let disk_str = instance
        .disk_path()
        .to_str()
        .context("disk path is not valid UTF-8")?
        .to_string();
    let seed_str = instance
        .seed_path()
        .to_str()
        .context("seed path is not valid UTF-8")?
        .to_string();
    let qmp_str = instance
        .qmp_socket_path()
        .to_str()
        .context("QMP socket path is not valid UTF-8")?
        .to_string();
    // Reject an over-long socket path here rather than letting QEMU take
    // it. QEMU's own length check is `>` where every client's is `>=`, so
    // at exactly `sizeof(sun_path)` it happily binds a path that nothing
    // can then connect to — verified on macOS at 104 bytes: QEMU creates
    // the socket, and both Rust and Python refuse it as "path too long".
    // The VM would start and then be uncontrollable, with the failure
    // surfacing much later as a confusing error from `stop` or `suspend`.
    //
    // `SocketAddr::from_pathname` applies exactly the check the client
    // will, so this stays correct on platforms with a different limit.
    std::os::unix::net::SocketAddr::from_pathname(instance.qmp_socket_path()).map_err(|_| {
        anyhow::anyhow!(
            "QMP socket path is too long for a unix socket ({} bytes): {qmp_str}\n\
             Use a shorter VM name, or set AGV_DATA_DIR to a shorter path.",
            qmp_str.len(),
        )
    })?;

    // Memory and CPUs.
    args.extend(["-m".to_string(), memory.to_string()]);
    args.extend([
        "-smp".to_string(),
        format!("cpus={cpus},cores={cpus},threads=1"),
    ]);

    // Disk drives.
    //
    // cache=writeback: uses host page cache, good balance of performance and
    // safety for ephemeral coding agent VMs.
    //
    // On Linux, aio=native with cache=none would be faster but requires
    // O_DIRECT support on the underlying filesystem. writeback is safe
    // everywhere and still much better than the default writethrough.
    args.extend([
        "-drive".to_string(),
        format!("file={disk_str},if=virtio,format=qcow2,cache=writeback"),
    ]);
    args.extend([
        "-drive".to_string(),
        format!("file={seed_str},if=virtio,media=cdrom"),
    ]);

    // Network with SSH port forwarding.
    args.extend([
        "-netdev".to_string(),
        format!("user,id=net0,hostfwd=tcp::{ssh_port}-:22"),
    ]);
    args.extend([
        "-device".to_string(),
        "virtio-net-pci,netdev=net0".to_string(),
    ]);

    // Hardware RNG — avoids guest stalls waiting for entropy during boot
    // and SSH key generation.
    args.extend(["-device".to_string(), "virtio-rng-pci".to_string()]);

    // QMP socket.
    args.extend([
        "-qmp".to_string(),
        format!("unix:{qmp_str},server,nowait"),
    ]);

    // Headless operation with serial console logged to file.
    let serial_str = instance
        .serial_log_path()
        .to_str()
        .context("serial log path is not valid UTF-8")?
        .to_string();
    args.extend([
        "-display".to_string(),
        "none".to_string(),
        "-serial".to_string(),
        format!("file:{serial_str}"),
        "-monitor".to_string(),
        "none".to_string(),
    ]);

    // Exit instead of rebooting when the guest halts or reboots. Without
    // this, `sudo halt` inside the VM leaves QEMU running (vCPU in halt
    // state, burning CPU). `-no-reboot` makes QEMU exit cleanly, and
    // status reconciliation will mark the VM as stopped.
    args.push("-no-reboot".to_string());

    // Deliberately no `-daemonize`. QEMU's own daemonize forks without
    // exec'ing, and on macOS any first-time Objective-C class
    // initialization in the forked child aborts the process — Apple's
    // runtime refuses it. That is reachable from `savevm` (HVF's GIC
    // state save goes through Hypervisor.framework, which is
    // Objective-C underneath), so a suspend on an Apple Silicon host
    // killed QEMU outright. Upstream has this confirmed and unfixed
    // since 2024: https://gitlab.com/qemu-project/qemu/-/work_items/2515
    //
    // agv detaches the process itself instead — see `spawn_detached`.
    // That also means no `-pidfile`: the PID is written from Rust, so
    // it is on disk before this function returns rather than whenever
    // QEMU gets around to it.

    Ok((binary, args))
}

/// Read the PID from the instance's PID file.
async fn read_pid(instance: &Instance) -> anyhow::Result<u32> {
    let path = instance.pid_path();
    let raw = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("failed to read PID file {}", path.display()))?;
    raw.trim()
        .parse::<u32>()
        .with_context(|| format!("invalid PID in {}: {raw:?}", path.display()))
}

/// How long to wait for QEMU to create its QMP socket before giving up.
///
/// Generous: this covers image probing and firmware load on a cold
/// host. A QEMU that is going to fail usually dies in well under a
/// second, and `wait_for_qmp_socket` notices the exit rather than
/// sitting out the full timeout.
const QMP_READY_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to let a QEMU that just closed its monitor socket actually
/// exit before deciding it is still alive.
const QEMU_EXIT_GRACE: Duration = Duration::from_secs(2);

/// Spawn QEMU as a detached child and record its PID.
///
/// Mirrors how agv already launches its other long-lived children
/// (forward supervisors, the idle watcher, the AVF runner): own process
/// group, stdio pointed somewhere durable, PID written from Rust. Unlike
/// those, the handle is kept and waited on — see the reaper below.
///
/// QEMU's stdout and stderr go to `<instance>/qemu.log`. Under
/// `-daemonize` they went to a pipe nobody read once startup
/// succeeded, so when QEMU died later — the Objective-C abort that
/// motivated this change, say — it printed a message naming the exact
/// problem and agv threw it away.
fn spawn_detached(instance: &Instance, binary: &str, args: &[String]) -> anyhow::Result<u32> {
    // Sweep a QMP socket left by an earlier boot. QEMU does not remove
    // its own on a failed start, and nothing else will: `reconcile_status`
    // only cleans up when the recorded status is `running`, so a VM left
    // `broken` with QEMU still running keeps the file once that QEMU
    // dies. Left in place it would satisfy the readiness check below on
    // the first poll, from the previous boot.
    let _ = std::fs::remove_file(instance.qmp_socket_path());

    let log_path = instance.qemu_log_path();
    // Truncate rather than append: one boot per file, matching
    // `serial.log` (which QEMU truncates via `-serial file:`) and the
    // idle watcher's log.
    let log = std::fs::File::create(&log_path)
        .with_context(|| format!("failed to create {}", log_path.display()))?;
    let log_clone = log.try_clone().context("failed to dup QEMU log fd")?;

    let mut cmd = tokio::process::Command::new(binary);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_clone));
    // Own process group, so a signal aimed at the VM can't travel back
    // up into the agv process that started it.
    cmd.process_group(0);
    // Explicit: dropping the handle must not take the VM with it.
    cmd.kill_on_drop(false);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("{binary} not found — run 'agv doctor' to check all dependencies");
        }
        Err(e) => {
            return Err(e).with_context(|| format!("failed to run {binary}"));
        }
    };
    let pid = child.id().context("QEMU exited before its PID could be read")?;

    // Reap the child rather than forgetting it. Under `-daemonize` QEMU
    // forked and the process agv spawned exited immediately, leaving the
    // real QEMU orphaned to init — which reaped it on exit, so a PID
    // check was always honest. Spawned directly, QEMU is agv's own
    // child, and a child nobody waits on becomes a zombie: the PID stays
    // in the process table and `kill(pid, 0)` keeps succeeding, so
    // `is_process_alive` would report a dead VM as running for as long
    // as the agv process lived. That matters wherever one invocation
    // both starts and stops a VM.
    //
    // The task only has to outlive agv's interest in the VM, not the VM
    // itself: if agv exits first, QEMU is orphaned to init exactly as
    // before and init does the reaping.
    //
    // The PID file is written first, while the handle is still ours: a
    // QEMU running with no PID file is unmanageable, since stop and
    // force_stop have nothing to signal while it holds the qcow2 write
    // lock and the allocated hostfwd port, so the next start fails on the
    // image lock too.
    if let Err(e) = std::fs::write(instance.pid_path(), pid.to_string()) {
        let _ = child.start_kill();
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
        return Err(e).with_context(|| {
            format!("failed to write PID file {}", instance.pid_path().display())
        });
    }

    tokio::spawn(async move {
        let _ = child.wait().await;
    });

    Ok(pid)
}

/// Why QEMU never became ready.
enum StartupFailure {
    /// QEMU exited on its own. Already reaped, so its PID must not be
    /// signalled — it may have been recycled.
    Exited(String),
    /// QEMU is still running but never served QMP, so it has to be killed.
    Stalled(String),
}

impl StartupFailure {
    fn reason(&self) -> &str {
        match self {
            Self::Exited(r) | Self::Stalled(r) => r,
        }
    }
}

/// Wait until QEMU is actually serving QMP, giving up early if it exits.
///
/// Deliberately a completed handshake rather than `socket_path.exists()`.
/// QEMU creates the QMP chardev socket very early in startup — before it
/// opens `-drive` files, binds the hostfwd port, realizes devices, or
/// applies `-loadvm` — and it does *not* unlink the socket when it then
/// exits with an error. Measured on an aarch64 host with a missing disk
/// image: socket present at 6.1ms, process gone with status 1 at 6.1ms,
/// socket still on disk afterwards. A file-existence check would call
/// that a successful start, which is strictly weaker than the exit
/// status `-daemonize` used to give us, and would leave the caller to
/// discover the failure later as an SSH timeout.
///
/// `QmpClient::connect` reads the greeting and sends `qmp_capabilities`,
/// so a connection that completes proves QEMU is up and past the point
/// where the errors above would have killed it. It also rejects a stale
/// socket left by a previous boot, which has no listener behind it.
async fn wait_for_qmp_ready(
    instance: &Instance,
    pid: u32,
    timeout: Duration,
) -> Result<(), StartupFailure> {
    let socket_path = instance.qmp_socket_path();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        // Liveness first: an exited QEMU can still have a socket file on
        // disk, so checking the socket first would report success for a
        // process that is already gone.
        if !is_process_alive(pid) {
            return Err(StartupFailure::Exited(format!(
                "QEMU exited before serving QMP on {}",
                socket_path.display()
            )));
        }
        if socket_path.exists() && QmpClient::connect(&socket_path).await.is_ok() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(StartupFailure::Stalled(format!(
                "QEMU did not serve QMP on {} within {timeout:?}",
                socket_path.display()
            )));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Poll until `pid` is gone, up to `timeout`. Returns whether it exited.
///
/// Exit is not instantaneous from the observer's side: a process that
/// aborts has its fds closed by the kernel — which is what shows up as an
/// EOF on the monitor socket — while `kill(pid, 0)` keeps succeeding until
/// it is reaped. Deciding on a single check right after an EOF therefore
/// tends to conclude "still alive" precisely when it isn't.
async fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if !is_process_alive(pid) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// SIGKILL a PID, ignoring failures.
///
/// Used on the startup-failure path, where QEMU may be wedged rather
/// than dead and there is no QMP socket to ask politely through.
fn force_kill(pid: u32) {
    if let Some(p) = crate::forward::pid_from_u32(pid) {
        let _ = rustix::process::kill_process(p, rustix::process::Signal::KILL);
    }
}

/// Build a single error message carrying the tail of `qemu.log`.
///
/// The whole point of capturing QEMU's output is that the user sees it
/// without going digging, so put it in the error rather than pointing at
/// a file path and hoping.
///
/// Flat rather than an `anyhow` context layer: `main.rs` renders errors
/// with `{err:#}`, which joins the chain with `": "` onto one line. A
/// multi-line log excerpt added as context therefore comes out ahead of
/// the reason it belongs to, reading as `<log dump>: <reason>`.
fn qemu_failure_message(reason: &str, log_path: &std::path::Path) -> String {
    // Cap the excerpt: a device-probe failure can be verbose, and the
    // line that matters is at the end.
    const MAX: usize = 2048;

    let log = std::fs::read_to_string(log_path).unwrap_or_default();
    let trimmed = log.trim();
    if trimmed.is_empty() {
        return format!("{reason} (no output in {})", log_path.display());
    }
    let tail = if trimmed.len() > MAX {
        let start = trimmed.len() - MAX;
        let safe = trimmed
            .char_indices()
            .find(|(i, _)| *i >= start)
            .map_or(trimmed.len(), |(i, _)| i);
        format!("...{}", &trimmed[safe..])
    } else {
        trimmed.to_string()
    };
    format!("{reason}\n--- {} ---\n{tail}", log_path.display())
}

/// Check whether a process with the given PID is alive.
fn is_process_alive(pid: u32) -> bool {
    crate::forward::pid_from_u32(pid)
        .is_some_and(|p| rustix::process::test_kill_process(p).is_ok())
}

/// Send a signal to a process. Returns `true` if the signal was sent
/// successfully, `false` if the process was not found.
fn kill_process(pid: u32, signal: rustix::process::Signal) -> bool {
    crate::forward::pid_from_u32(pid)
        .is_some_and(|p| rustix::process::kill_process(p, signal).is_ok())
}

/// Remove runtime files (PID, QMP socket, SSH port).
///
/// Best-effort cleanup: a missing file is expected (nothing to remove), but
/// any unexpected error is logged at debug level so it surfaces in verbose
/// mode without noise in the normal path.
async fn cleanup_runtime_files(instance: &Instance) {
    for path in [
        instance.pid_path(),
        instance.qmp_socket_path(),
        instance.ssh_port_path(),
    ] {
        if let Err(e) = tokio::fs::remove_file(&path).await
            && e.kind() != std::io::ErrorKind::NotFound
        {
            debug!(path = %path.display(), error = %e, "cleanup: failed to remove runtime file");
        }
    }
    // Use the supervisor-aware cleanup so any leftover forward supervisors
    // are torn down (vm::stop usually does this earlier, but cleanup runs
    // from QEMU-only paths too — e.g. force_stop after a stale PID).
    crate::forward::kill_all_and_clear(&instance.forwards_path()).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn allocate_free_port_returns_nonzero() {
        let port = allocate_free_port().await.unwrap();
        assert!(port > 0, "expected non-zero port, got {port}");
    }

    #[tokio::test]
    async fn allocate_free_port_returns_unique_ports() {
        let port1 = allocate_free_port().await.unwrap();
        let port2 = allocate_free_port().await.unwrap();
        assert_ne!(port1, port2, "expected unique ports, got {port1} twice");
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn detect_image_format_qcow2() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.qcow2");
        // qcow2 magic: "QFI\xfb" followed by version etc.
        std::fs::write(&path, b"QFI\xfbsome more bytes").unwrap();
        assert_eq!(detect_image_format(&path).unwrap(), "qcow2");
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn detect_image_format_raw() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.raw");
        // EFI vars file starts with EFI vars header, not qcow2 magic.
        std::fs::write(&path, b"\x00\x00\x00\x00random binary content").unwrap();
        assert_eq!(detect_image_format(&path).unwrap(), "raw");
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn detect_image_format_short_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        std::fs::write(&path, b"ab").unwrap(); // shorter than 4 bytes
        assert_eq!(detect_image_format(&path).unwrap(), "raw");
    }

    #[test]
    fn platform_args_returns_expected_binary() {
        let p = platform_args().unwrap();

        if cfg!(target_arch = "aarch64") {
            assert_eq!(p.binary, "qemu-system-aarch64");
            assert_eq!(p.machine_alias, "virt");
        } else if cfg!(target_arch = "x86_64") {
            assert_eq!(p.binary, "qemu-system-x86_64");
            assert_eq!(p.machine_alias, "q35");
        }

        // Should contain an accelerator.
        assert!(
            p.accel_and_cpu_args.contains(&"hvf".to_string())
                || p.accel_and_cpu_args.contains(&"kvm".to_string()),
            "expected hvf or kvm in accel_and_cpu_args: {:?}",
            p.accel_and_cpu_args
        );
    }

    #[test]
    fn build_qemu_args_contains_required_flags() {
        let dir = tempfile::tempdir().unwrap();
        let instance = Instance {
            name: "test-build-args".to_string(),
            dir: dir.path().to_path_buf(),
        };

        // build_qemu_args may fail on platforms without EFI firmware,
        // which is fine — we only test the flag content when it succeeds.
        let Ok((binary, args)) = build_qemu_args(&instance, "2G", 4, 2222, "pc-q35-9.2") else {
            eprintln!("skipping build_qemu_args test (EFI firmware not found)");
            return;
        };

        assert!(!binary.is_empty());

        let joined = args.join(" ");
        assert!(joined.contains("-m 2G"), "missing -m flag: {joined}");
        assert!(
            joined.contains("-smp cpus=4,cores=4,threads=1"),
            "missing -smp flag: {joined}"
        );
        assert!(
            joined.contains("hostfwd=tcp::2222-:22"),
            "missing hostfwd: {joined}"
        );
        assert!(joined.contains("-no-reboot"), "missing -no-reboot: {joined}");
        // No `-daemonize` / `-pidfile`: agv detaches QEMU itself and
        // writes the PID, because QEMU's own daemonize fork makes a
        // later `savevm` abort on macOS. See `spawn_detached`.
        assert!(
            !joined.contains("-daemonize"),
            "-daemonize must not be passed: {joined}"
        );
        assert!(
            !joined.contains("-pidfile"),
            "-pidfile must not be passed: {joined}"
        );
        assert!(
            joined.contains("pc-q35-9.2"),
            "missing pinned machine type in args: {joined}"
        );
        assert!(
            joined.contains("disk.qcow2"),
            "missing disk path: {joined}"
        );
        assert!(
            joined.contains("seed.iso"),
            "missing seed path: {joined}"
        );
        assert!(
            joined.contains("qmp.sock"),
            "missing QMP socket: {joined}"
        );
    }

    #[test]
    fn qmp_greeting_has_expected_shape() {
        let greeting: serde_json::Value =
            serde_json::from_str(r#"{"QMP":{"version":{"qemu":{"micro":0,"minor":2,"major":9},"package":""},"capabilities":[]}}"#).unwrap();
        assert!(greeting.get("QMP").is_some());
    }

    #[test]
    fn qmp_success_response_is_recognized() {
        let resp: serde_json::Value = serde_json::from_str(r#"{"return":{}}"#).unwrap();
        assert!(resp.get("return").is_some());
        assert!(resp.get("error").is_none());
    }

    #[test]
    fn qmp_error_response_is_recognized() {
        let resp: serde_json::Value = serde_json::from_str(
            r#"{"error":{"class":"GenericError","desc":"command not found"}}"#,
        )
        .unwrap();
        assert!(resp.get("error").is_some());
        assert!(resp.get("return").is_none());
    }

    #[test]
    fn qmp_event_is_not_a_command_response() {
        let event: serde_json::Value = serde_json::from_str(
            r#"{"event":"POWERDOWN","timestamp":{"seconds":1234,"microseconds":0},"data":{}}"#,
        )
        .unwrap();
        assert!(event.get("event").is_some());
        assert!(event.get("return").is_none());
        assert!(event.get("error").is_none());
    }

    #[tokio::test]
    async fn cleanup_runtime_files_tolerates_missing() {
        let dir = tempfile::tempdir().unwrap();
        let instance = Instance {
            name: "test-cleanup-empty".to_string(),
            dir: dir.path().to_path_buf(),
        };
        // Should not panic on an empty directory.
        cleanup_runtime_files(&instance).await;
    }

    #[tokio::test]
    async fn cleanup_runtime_files_removes_existing() {
        let dir = tempfile::tempdir().unwrap();
        let instance = Instance {
            name: "test-cleanup".to_string(),
            dir: dir.path().to_path_buf(),
        };

        // Write the files that cleanup should remove.
        tokio::fs::write(instance.pid_path(), "12345").await.unwrap();
        tokio::fs::write(instance.qmp_socket_path(), "dummy").await.unwrap();
        tokio::fs::write(instance.ssh_port_path(), "2222").await.unwrap();
        tokio::fs::write(instance.forwards_path(), "active = []\n").await.unwrap();

        assert!(instance.pid_path().exists());
        assert!(instance.qmp_socket_path().exists());
        assert!(instance.ssh_port_path().exists());

        cleanup_runtime_files(&instance).await;

        assert!(!instance.pid_path().exists());
        assert!(!instance.qmp_socket_path().exists());
        assert!(!instance.ssh_port_path().exists());
        assert!(!instance.forwards_path().exists());
    }

    #[tokio::test]
    async fn read_pid_valid() {
        let dir = tempfile::tempdir().unwrap();
        let instance = Instance {
            name: "test-pid".to_string(),
            dir: dir.path().to_path_buf(),
        };

        tokio::fs::write(instance.pid_path(), "42\n").await.unwrap();
        let pid = read_pid(&instance).await.unwrap();
        assert_eq!(pid, 42);
    }

    #[tokio::test]
    async fn read_pid_missing_errors() {
        let dir = tempfile::tempdir().unwrap();
        let instance = Instance {
            name: "test-no-pid".to_string(),
            dir: dir.path().to_path_buf(),
        };

        let result = read_pid(&instance).await;
        assert!(result.is_err());
        let err = format!("{:#}", result.unwrap_err());
        assert!(
            err.contains("failed to read PID file"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parse_machine_help_resolves_q35_alias() {
        let help = "\
Supported machines are:
none                 empty machine
pc                   Standard PC (i440FX + PIIX, 1996) (alias of pc-i440fx-9.2)
pc-i440fx-9.2        Standard PC (i440FX + PIIX, 1996) (default)
pc-i440fx-9.1        Standard PC (i440FX + PIIX, 1996) (deprecated)
q35                  Standard PC (Q35 + ICH9, 2009) (alias of pc-q35-9.2)
pc-q35-9.2           Standard PC (Q35 + ICH9, 2009)
pc-q35-9.1           Standard PC (Q35 + ICH9, 2009)
microvm              microvm (i386)
";
        assert_eq!(
            parse_machine_help(help, "q35").as_deref(),
            Some("pc-q35-9.2"),
        );
    }

    #[test]
    fn parse_machine_help_resolves_virt_alias() {
        let help = "\
Supported machines are:
none                 empty machine
virt                 QEMU 9.2 ARM Virtual Machine (alias of virt-9.2)
virt-9.2             QEMU 9.2 ARM Virtual Machine
virt-9.1             QEMU 9.1 ARM Virtual Machine
virt-8.2             QEMU 8.2 ARM Virtual Machine (deprecated)
";
        assert_eq!(
            parse_machine_help(help, "virt").as_deref(),
            Some("virt-9.2"),
        );
    }

    #[test]
    fn parse_machine_help_falls_back_to_highest_versioned() {
        // No `(alias of ...)` annotation — older QEMU layout. We should
        // still pick the highest non-deprecated version.
        let help = "\
Supported machines are:
q35                  Standard PC (Q35 + ICH9, 2009)
pc-q35-8.2           Standard PC (Q35 + ICH9, 2009)
pc-q35-9.2           Standard PC (Q35 + ICH9, 2009)
pc-q35-9.1           Standard PC (Q35 + ICH9, 2009)
pc-q35-8.0           Standard PC (Q35 + ICH9, 2009) (deprecated)
";
        assert_eq!(
            parse_machine_help(help, "q35").as_deref(),
            Some("pc-q35-9.2"),
        );
    }

    #[test]
    fn parse_machine_help_returns_none_when_alias_absent() {
        let help = "\
Supported machines are:
none                 empty machine
";
        assert_eq!(parse_machine_help(help, "q35"), None);
        assert_eq!(parse_machine_help(help, "virt"), None);
    }
}
