//! Integration tests for the VM create lifecycle.
//!
//! These tests exercise `vm::create()` end-to-end, requiring external tools
//! (`qemu-img`, `mkisofs`/`genisoimage`). Tests skip gracefully if tools are
//! missing, so `cargo test` always passes.
//!
//! Each test creates its own dummy base image in the agv image cache using
//! a unique filename, so tests never overwrite real cached images or conflict
//! with each other.

use std::sync::atomic::{AtomicU32, Ordering};

use agv::vm::instance::{Instance, Status};
use agv::{config, dirs, vm};

/// Counter to generate unique filenames across concurrent tests.
static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Check whether `qemu-img` is available.
fn qemu_img_available() -> bool {
    std::process::Command::new("qemu-img")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Check whether an ISO generation tool (`mkisofs` or `genisoimage`) is available.
fn iso_tool_available() -> bool {
    for tool in ["mkisofs", "genisoimage"] {
        if std::process::Command::new(tool)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
        {
            return true;
        }
    }
    false
}

/// Check whether the platform-appropriate QEMU system binary is available.
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

/// Create a unique dummy qcow2 in the image cache and return a fake URL
/// whose filename matches the cached file. Each call produces a unique
/// filename so concurrent tests never collide.
async fn create_test_base_image() -> String {
    let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let filename = format!("_agv-test-base-{id}.qcow2");
    let fake_url = format!("https://example.invalid/{filename}");

    let cache_dir = dirs::image_cache_dir().unwrap();
    tokio::fs::create_dir_all(&cache_dir).await.unwrap();

    let cached_path = cache_dir.join(&filename);
    let cached_str = cached_path.to_str().unwrap();
    let output = tokio::process::Command::new("qemu-img")
        .args(["create", "-f", "qcow2", cached_str, "1G"])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "qemu-img create failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    fake_url
}

/// Build a minimal Config that uses the given test image URL.
fn test_config(image_url: &str) -> config::Config {
    config::Config {
        vm: Some(config::VmConfig {
            name: None,
            memory: Some("512M".to_string()),
            cpus: Some(1),
            disk: Some("2G".to_string()),
            user: Some("agent".to_string()),
            image: Some(image_url.to_string()),
            image_checksum: None,
        }),
        files: vec![],
        provision: vec![],
    }
}

/// Force-destroy a VM, ignoring errors (best-effort cleanup).
async fn cleanup(name: &str) {
    let _ = vm::destroy(name).await;
}

#[tokio::test]
async fn create_without_start() {
    if !qemu_img_available() {
        eprintln!("qemu-img not installed — skipping create_without_start");
        return;
    }
    if !iso_tool_available() {
        eprintln!("mkisofs/genisoimage not installed — skipping create_without_start");
        return;
    }

    let image_url = create_test_base_image().await;

    let name = "_test-create-nostrt";
    cleanup(name).await;

    let config = test_config(&image_url);
    vm::create(name, &config, false).await.unwrap();

    // Verify instance directory and files exist.
    let inst_dir = dirs::instance_dir(name).unwrap();
    assert!(inst_dir.exists(), "instance dir should exist");

    let inst = Instance {
        name: name.to_string(),
        dir: inst_dir,
    };

    assert!(inst.disk_path().exists(), "disk.qcow2 should exist");
    assert!(inst.seed_path().exists(), "seed.iso should exist");
    assert!(inst.ssh_key_path().exists(), "SSH private key should exist");
    assert!(
        inst.ssh_pub_key_path().exists(),
        "SSH public key should exist"
    );
    assert!(inst.config_path().exists(), "config.toml should exist");

    // Status should be stopped (not started).
    let status = inst.read_status().await.unwrap();
    assert_eq!(status, Status::Stopped);

    // PID and port files should NOT exist (not started).
    assert!(!inst.pid_path().exists(), "PID file should not exist");
    assert!(
        !inst.ssh_port_path().exists(),
        "SSH port file should not exist"
    );

    // Saved config should be loadable.
    let saved = config::load(&inst.config_path()).unwrap();
    let vm = saved.vm.unwrap();
    assert_eq!(vm.memory.as_deref(), Some("512M"));
    assert_eq!(vm.cpus, Some(1));

    cleanup(name).await;
}

#[tokio::test]
async fn create_duplicate_name_fails() {
    if !qemu_img_available() {
        eprintln!("qemu-img not installed — skipping create_duplicate_name_fails");
        return;
    }
    if !iso_tool_available() {
        eprintln!("mkisofs/genisoimage not installed — skipping create_duplicate_name_fails");
        return;
    }

    let image_url = create_test_base_image().await;

    let name = "_test-create-dup";
    cleanup(name).await;

    let config = test_config(&image_url);

    // First create should succeed.
    vm::create(name, &config, false).await.unwrap();

    // Second create with same name should fail with VmAlreadyExists.
    let result = vm::create(name, &config, false).await;
    assert!(result.is_err());
    let err = format!("{:#}", result.unwrap_err());
    assert!(
        err.contains("already exists"),
        "expected 'already exists' error, got: {err}"
    );

    cleanup(name).await;
}

#[tokio::test]
async fn create_marks_broken_on_failure() {
    dirs::ensure_dirs().await.unwrap();

    let name = "_test-create-broken";
    cleanup(name).await;

    // Build a config that points to a nonexistent image URL — this will fail
    // during the image download/cache step.
    let config = config::Config {
        vm: Some(config::VmConfig {
            name: None,
            memory: Some("512M".to_string()),
            cpus: Some(1),
            disk: Some("2G".to_string()),
            user: Some("agent".to_string()),
            image: Some("http://127.0.0.1:1/nonexistent-image.qcow2".to_string()),
            image_checksum: None,
        }),
        files: vec![],
        provision: vec![],
    };

    // Create should fail (unreachable image URL).
    let result = vm::create(name, &config, false).await;
    assert!(result.is_err(), "create should fail with bad image URL");

    // Instance dir should still exist.
    let inst_dir = dirs::instance_dir(name).unwrap();
    assert!(inst_dir.exists(), "instance dir should exist after failure");

    let inst = Instance {
        name: name.to_string(),
        dir: inst_dir,
    };

    // Status should be broken.
    let status = inst.read_status().await.unwrap();
    assert_eq!(status, Status::Broken);

    // Error log should exist with some content.
    assert!(inst.error_log_path().exists(), "error.log should exist");
    let error_log = tokio::fs::read_to_string(inst.error_log_path())
        .await
        .unwrap();
    assert!(!error_log.is_empty(), "error.log should have content");

    cleanup(name).await;
}

/// Full create-start-provision lifecycle test. Marked `#[ignore]` because it
/// downloads a real cloud image and boots a VM — slow and requires all tools.
#[tokio::test]
#[ignore = "requires real cloud image download and full QEMU boot — slow"]
async fn create_with_start_and_provision() {
    if !qemu_img_available() || !iso_tool_available() || !qemu_available() {
        eprintln!("required tools not installed — skipping create_with_start_and_provision");
        return;
    }

    dirs::ensure_dirs().await.unwrap();

    let name = "_test-create-full";
    cleanup(name).await;

    let config = config::Config {
        vm: Some(config::VmConfig {
            name: None,
            memory: Some("1G".to_string()),
            cpus: Some(2),
            disk: Some("10G".to_string()),
            user: Some("agent".to_string()),
            image: None, // uses default Ubuntu image
            image_checksum: None,
        }),
        files: vec![],
        provision: vec![config::ProvisionStep {
            run: Some("echo 'provisioning complete' > /tmp/agv-test-marker".to_string()),
            script: None,
        }],
    };

    vm::create(name, &config, true).await.unwrap();

    let inst_dir = dirs::instance_dir(name).unwrap();
    let inst = Instance {
        name: name.to_string(),
        dir: inst_dir,
    };

    let status = inst.read_status().await.unwrap();
    assert_eq!(status, Status::Running);
    assert!(inst.pid_path().exists());
    assert!(inst.ssh_port_path().exists());

    cleanup(name).await;
}
