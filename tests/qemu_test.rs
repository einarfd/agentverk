//! Integration tests for QEMU process spawning and management.
//!
//! These tests spawn a real QEMU process and verify lifecycle management.
//! They skip gracefully if QEMU is not installed, so `cargo test` always
//! passes on CI systems without QEMU.

use std::path::PathBuf;

use agv::forward::{ForwardSpec, Proto};
use agv::vm::instance::Instance;
use agv::vm::{cloud_init, qemu};

/// Check whether the platform-appropriate QEMU binary is available.
fn qemu_available() -> bool {
    let binary = if cfg!(target_arch = "aarch64") {
        "qemu-system-aarch64"
    } else {
        "qemu-system-x86_64"
    };

    std::process::Command::new(binary)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Create a minimal test instance with a small empty disk and seed ISO.
async fn setup_instance(dir: &std::path::Path, name: &str) -> anyhow::Result<Instance> {
    let instance = Instance {
        name: name.to_string(),
        dir: dir.to_path_buf(),
    };

    // Create a small empty qcow2 disk.
    let disk_str = instance
        .disk_path()
        .to_str()
        .expect("disk path is valid UTF-8")
        .to_string();
    let output = tokio::process::Command::new("qemu-img")
        .args(["create", "-f", "qcow2", &disk_str, "1G"])
        .output()
        .await?;
    assert!(output.status.success(), "qemu-img create failed");

    // Generate a seed ISO with a dummy SSH key.
    cloud_init::generate_seed(
        &instance.seed_path(),
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITestKeyForQemuIntegrationTests test@agv",
        name,
        "agent",
    )
    .await?;

    Ok(instance)
}

/// Verify a PID file contains a valid, alive process ID.
async fn assert_pid_alive(pid_path: &PathBuf) {
    assert!(pid_path.exists(), "PID file should exist");
    let raw = tokio::fs::read_to_string(pid_path).await.unwrap();
    let pid: u32 = raw.trim().parse().expect("PID should be a valid u32");
    assert!(pid > 0, "PID should be non-zero");

    let alive = std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    assert!(alive, "QEMU process {pid} should be alive");
}

#[tokio::test]
async fn qemu_start_and_force_stop() {
    if !qemu_available() {
        eprintln!("QEMU not installed — skipping qemu_start_and_force_stop");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let instance = setup_instance(dir.path(), "test-force-stop").await.unwrap();

    // Start QEMU.
    qemu::start(&instance, "512M", 1).await.unwrap();

    // Verify start artifacts.
    assert_pid_alive(&instance.pid_path()).await;
    assert!(
        instance.ssh_port_path().exists(),
        "SSH port file should exist"
    );
    assert!(
        instance.qmp_socket_path().exists(),
        "QMP socket should exist"
    );

    let port_raw = tokio::fs::read_to_string(instance.ssh_port_path())
        .await
        .unwrap();
    let port: u16 = port_raw.trim().parse().unwrap();
    assert!(port > 0, "SSH port should be non-zero");

    // Read PID before stopping for verification.
    let pid_raw = tokio::fs::read_to_string(instance.pid_path())
        .await
        .unwrap();
    let pid: u32 = pid_raw.trim().parse().unwrap();

    // Force stop.
    qemu::force_stop(&instance).await.unwrap();

    // Verify cleanup.
    assert!(
        !instance.pid_path().exists(),
        "PID file should be cleaned up"
    );
    assert!(
        !instance.ssh_port_path().exists(),
        "SSH port file should be cleaned up"
    );

    // Process should be gone.
    let still_alive = std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    assert!(!still_alive, "QEMU process should be dead after force_stop");
}

#[tokio::test]
async fn qemu_start_and_graceful_stop() {
    if !qemu_available() {
        eprintln!("QEMU not installed — skipping qemu_start_and_graceful_stop");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let instance = setup_instance(dir.path(), "test-graceful-stop")
        .await
        .unwrap();

    // Start QEMU.
    qemu::start(&instance, "512M", 1).await.unwrap();

    // Verify it started.
    assert_pid_alive(&instance.pid_path()).await;

    let pid_raw = tokio::fs::read_to_string(instance.pid_path())
        .await
        .unwrap();
    let pid: u32 = pid_raw.trim().parse().unwrap();

    // Graceful stop (will send system_powerdown via QMP, then fall back to force kill
    // after timeout since we have no bootable OS).
    qemu::stop(&instance).await.unwrap();

    // Verify cleanup.
    assert!(
        !instance.pid_path().exists(),
        "PID file should be cleaned up"
    );

    let still_alive = std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    assert!(
        !still_alive,
        "QEMU process should be dead after graceful stop"
    );
}

#[tokio::test]
async fn qemu_start_writes_pid_and_port() {
    if !qemu_available() {
        eprintln!("QEMU not installed — skipping qemu_start_writes_pid_and_port");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let instance = setup_instance(dir.path(), "test-artifacts").await.unwrap();

    qemu::start(&instance, "512M", 1).await.unwrap();

    // Verify PID file.
    let pid_raw = tokio::fs::read_to_string(instance.pid_path())
        .await
        .unwrap();
    let pid: u32 = pid_raw.trim().parse().unwrap();
    assert!(pid > 1, "PID should be a valid process ID");

    // Verify SSH port file.
    let port_raw = tokio::fs::read_to_string(instance.ssh_port_path())
        .await
        .unwrap();
    let port: u16 = port_raw.trim().parse().unwrap();
    assert!(port > 1024, "SSH port should be an unprivileged port");

    // Verify QMP socket.
    assert!(instance.qmp_socket_path().exists(), "QMP socket should exist");

    // Clean up — force stop the running process.
    qemu::force_stop(&instance).await.unwrap();
}

#[tokio::test]
async fn hostfwd_add_and_remove_via_qmp() {
    if !qemu_available() {
        eprintln!("QEMU not installed — skipping hostfwd_add_and_remove_via_qmp");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let instance = setup_instance(dir.path(), "test-hostfwd").await.unwrap();

    qemu::start(&instance, "512M", 1).await.unwrap();
    // Give QEMU a brief moment to finish initializing QMP before we connect.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Pick a port that's likely free; QEMU will reject if taken, which is
    // an acceptable failure mode for this smoke test.
    let spec = ForwardSpec::new(0, 22, Proto::Tcp);
    let chosen_host_port = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    };
    let spec = ForwardSpec { host: chosen_host_port, ..spec };

    qemu::hostfwd_add(&instance, spec).await.expect("hostfwd_add should succeed");

    // Re-adding the same forward should fail — QEMU rejects duplicates.
    let dup = qemu::hostfwd_add(&instance, spec).await;
    assert!(dup.is_err(), "duplicate hostfwd_add should fail, got {dup:?}");

    qemu::hostfwd_remove(&instance, spec)
        .await
        .expect("hostfwd_remove should succeed");

    // QEMU is lenient about removing an already-gone rule (returns nothing);
    // our wrapper mirrors that — double-remove is not an error.
    qemu::hostfwd_remove(&instance, spec)
        .await
        .expect("second hostfwd_remove should be a no-op, not fail");

    qemu::force_stop(&instance).await.unwrap();
}
