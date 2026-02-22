//! QEMU process spawning and QMP protocol communication.
//!
//! Handles starting QEMU as a background process, communicating over the
//! QMP JSON socket for lifecycle management, and graceful/forceful shutdown.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{debug, info, warn};

use crate::vm::instance::Instance;

/// Spawn a QEMU process for the given VM instance.
///
/// Allocates a free port for SSH forwarding, builds the QEMU command line,
/// and spawns QEMU in daemon mode. On success, the PID file and SSH port
/// file are written by QEMU and this function respectively.
pub async fn start(instance: &Instance, memory: &str, cpus: u32) -> anyhow::Result<()> {
    let ssh_port = allocate_free_port().await?;
    let (binary, args) = build_qemu_args(instance, memory, cpus, ssh_port)?;

    info!(
        vm = %instance.name,
        binary = %binary,
        ssh_port,
        "starting QEMU"
    );

    let result = tokio::process::Command::new(&binary)
        .args(&args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .await;

    match result {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("QEMU failed to start (exit {}): {stderr}", output.status);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("{binary} not found — is QEMU installed?");
        }
        Err(e) => {
            return Err(e).with_context(|| format!("failed to run {binary}"));
        }
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

    let mut client = QmpClient::connect(&socket_path).await?;
    client.execute("system_powerdown").await?;

    // Poll for process exit, up to 30 seconds.
    let pid = read_pid(instance).await?;
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

/// Force-kill the QEMU process using the PID file.
pub async fn force_stop(instance: &Instance) -> anyhow::Result<()> {
    let Ok(pid) = read_pid(instance).await else {
        debug!(vm = %instance.name, "no PID file found — process already gone");
        cleanup_runtime_files(instance).await;
        return Ok(());
    };

    info!(vm = %instance.name, pid, "force-killing QEMU process");
    let _ = kill_process(pid, "KILL");

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
async fn allocate_free_port() -> anyhow::Result<u16> {
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

/// Return the QEMU binary name and platform-specific machine/accel args.
#[allow(clippy::unnecessary_wraps)] // Returns Err on unsupported platforms (compile-time).
fn platform_args() -> anyhow::Result<(String, Vec<String>)> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        Ok((
            "qemu-system-aarch64".to_string(),
            vec![
                "-machine".to_string(),
                "virt".to_string(),
                "-accel".to_string(),
                "hvf".to_string(),
                "-cpu".to_string(),
                "host".to_string(),
            ],
        ))
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        let nested = nested_virt_available();
        if nested {
            info!("nested virtualization: enabled (host KVM module supports it)");
        }
        Ok((
            "qemu-system-x86_64".to_string(),
            vec![
                "-machine".to_string(),
                "q35".to_string(),
                "-accel".to_string(),
                "kvm".to_string(),
                "-cpu".to_string(),
                "host".to_string(),
            ],
        ))
    }

    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        let nested = nested_virt_available();
        let machine = if nested {
            info!("nested virtualization: enabled (virtualization=on)");
            "virt,virtualization=on"
        } else {
            "virt"
        };
        Ok((
            "qemu-system-aarch64".to_string(),
            vec![
                "-machine".to_string(),
                machine.to_string(),
                "-accel".to_string(),
                "kvm".to_string(),
                "-cpu".to_string(),
                "host".to_string(),
            ],
        ))
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
fn build_qemu_args(
    instance: &Instance,
    memory: &str,
    cpus: u32,
    ssh_port: u16,
) -> anyhow::Result<(String, Vec<String>)> {
    let (binary, mut args) = platform_args()?;

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
        // Copy the vars template if this instance doesn't have one yet.
        if !vars_dst.exists() {
            std::fs::copy(&firmware.vars, &vars_dst).with_context(|| {
                format!(
                    "failed to copy EFI vars template {} → {}",
                    firmware.vars.display(),
                    vars_dst.display()
                )
            })?;
        }
        let vars_str = vars_dst
            .to_str()
            .context("EFI vars path is not valid UTF-8")?;
        args.extend([
            "-drive".to_string(),
            format!("if=pflash,format=raw,readonly=on,file={code_str}"),
            "-drive".to_string(),
            format!("if=pflash,format=raw,file={vars_str}"),
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
    let pid_str = instance
        .pid_path()
        .to_str()
        .context("PID file path is not valid UTF-8")?
        .to_string();

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

    // Daemonize and write PID.
    args.extend([
        "-daemonize".to_string(),
        "-pidfile".to_string(),
        pid_str,
    ]);

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

/// Check whether a process with the given PID is alive.
fn is_process_alive(pid: u32) -> bool {
    // Use kill -0 via std::process since we just need a quick synchronous check.
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Send a signal to a process. Returns `true` if the signal was sent
/// successfully, `false` if the process was not found.
fn kill_process(pid: u32, signal: &str) -> bool {
    std::process::Command::new("kill")
        .args([&format!("-{signal}"), &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Remove runtime files (PID, QMP socket, SSH port) — ignore errors.
async fn cleanup_runtime_files(instance: &Instance) {
    let _ = tokio::fs::remove_file(instance.pid_path()).await;
    let _ = tokio::fs::remove_file(instance.qmp_socket_path()).await;
    let _ = tokio::fs::remove_file(instance.ssh_port_path()).await;
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

    #[test]
    fn platform_args_returns_expected_binary() {
        let (binary, args) = platform_args().unwrap();

        if cfg!(target_arch = "aarch64") {
            assert_eq!(binary, "qemu-system-aarch64");
        } else if cfg!(target_arch = "x86_64") {
            assert_eq!(binary, "qemu-system-x86_64");
        }

        // Should contain an accelerator.
        assert!(
            args.contains(&"hvf".to_string()) || args.contains(&"kvm".to_string()),
            "expected hvf or kvm in args: {args:?}"
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
        let Ok((binary, args)) = build_qemu_args(&instance, "2G", 4, 2222) else {
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
        assert!(joined.contains("-daemonize"), "missing -daemonize: {joined}");
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

        assert!(instance.pid_path().exists());
        assert!(instance.qmp_socket_path().exists());
        assert!(instance.ssh_port_path().exists());

        cleanup_runtime_files(&instance).await;

        assert!(!instance.pid_path().exists());
        assert!(!instance.qmp_socket_path().exists());
        assert!(!instance.ssh_port_path().exists());
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
}
