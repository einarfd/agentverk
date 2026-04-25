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

use agv::vm::instance::{Instance, Phase, Status};
use agv::{config, dirs, ssh, vm};
use serial_test::serial;

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

/// Build a minimal `ResolvedConfig` that uses the given test image URL.
fn test_config(image_url: &str) -> config::ResolvedConfig {
    config::ResolvedConfig {
        base_url: image_url.to_string(),
        base_checksum: "sha256:test".to_string(),
        skip_checksum: true,
        memory: "512M".to_string(),
        cpus: 1,
        disk: "2G".to_string(),
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
    }
}

/// Force-destroy a VM, ignoring errors (best-effort cleanup).
async fn cleanup(name: &str) {
    let _ = vm::destroy(name, true).await;
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
    vm::create(name, &config, false, false, false, true).await.unwrap();

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

    // Saved config should be loadable as a ResolvedConfig.
    let saved = config::load_resolved(&inst.config_path()).unwrap();
    assert_eq!(saved.memory, "512M");
    assert_eq!(saved.cpus, 1);

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
    vm::create(name, &config, false, false, false, true).await.unwrap();

    // Second create with same name should fail with VmAlreadyExists.
    let result = vm::create(name, &config, false, false, false, true).await;
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
    let config = config::ResolvedConfig {
        base_url: "http://127.0.0.1:1/nonexistent-image.qcow2".to_string(),
        base_checksum: "sha256:test".to_string(),
        skip_checksum: true,
        memory: "512M".to_string(),
        cpus: 1,
        disk: "2G".to_string(),
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
    };

    // Create should fail (unreachable image URL).
    let result = vm::create(name, &config, false, false, false, true).await;
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

/// Full create-start-provision lifecycle test using debian-12 (smaller image,
/// faster download than Ubuntu).
///
/// Ignored by default because it downloads a real cloud image and boots a VM.
/// Requires QEMU, qemu-img, and mkisofs/genisoimage.
///
/// Run with:
///   cargo test `create_with_start_and_provision` -- --include-ignored --nocapture
#[tokio::test]
#[ignore = "downloads a real cloud image and boots a VM — slow"]
#[serial(vm_boot)]
async fn create_with_start_and_provision() {
    if !qemu_img_available() || !iso_tool_available() || !qemu_available() {
        eprintln!("required tools not installed — skipping create_with_start_and_provision");
        return;
    }

    dirs::ensure_dirs().await.unwrap();

    let name = "_test-create-full";
    cleanup(name).await;

    // Create a temp file on the host to test file injection via SCP.
    let tmp_dir = tempfile::tempdir().unwrap();
    let test_file = tmp_dir.path().join("agv-test-inject.txt");
    tokio::fs::write(&test_file, "injected-by-agv")
        .await
        .unwrap();

    // Use debian-12: smaller image (~330 MB) and fully apt-compatible.
    let config = config::resolve(config::Config {
        base: Some(config::BaseConfig {
            from: Some("debian-12".to_string()),
            ..Default::default()
        }),
        vm: Some(config::VmConfig {
            memory: Some("1G".to_string()),
            cpus: Some(2),
            disk: Some("10G".to_string()),
        }),
        files: vec![config::FileEntry {
            source: test_file.to_str().unwrap().to_string(),
            dest: "/home/agent/.config/agv-test/agv-test-inject.txt".to_string(),
            optional: false,
        }],
        setup: vec![],
        provision: vec![config::ProvisionStep {
            source: None,
            run: Some(
                "cat /home/agent/.config/agv-test/agv-test-inject.txt".to_string(),
            ),
            script: None,
        }],
        forwards: vec![],
    os_families: None,
    supports: None,
    auto_forwards: None,
    notes: vec![],
    manual_steps: vec![],
    })
    .unwrap();

    assert!(!config.provision.is_empty());

    vm::create(name, &config, true, false, false, true).await.unwrap();

    let inst_dir = dirs::instance_dir(name).unwrap();
    let inst = Instance {
        name: name.to_string(),
        dir: inst_dir,
    };

    let status = inst.read_status().await.unwrap();
    assert_eq!(status, Status::Running);
    assert!(inst.pid_path().exists());
    assert!(inst.ssh_port_path().exists());

    // Verify provision.log was written and contains output.
    assert!(inst.provision_log_path().exists(), "provision.log should exist");
    let log = tokio::fs::read_to_string(inst.provision_log_path()).await.unwrap();
    assert!(!log.is_empty(), "provision.log should have content");
    // The provision step cats the injected file into /tmp/agv-test-marker.
    // If the file was copied correctly, the log should contain the injected content.
    assert!(
        log.contains("injected-by-agv"),
        "provision.log should contain injected file content — file copy via SCP failed"
    );

    // Verify the provisioned marker was set.
    assert!(inst.is_provisioned(), "instance should be marked provisioned");

    cleanup(name).await;
}

/// Same happy-path check as `create_with_start_and_provision`, but against
/// the `fedora-43` base to verify non-debian families boot and provision
/// end-to-end: cloud-init applies our seed, SSH comes up, file injection
/// via SCP works, and a provision step runs successfully.
///
/// Also exercises the `[os_families.fedora]` dispatch on the `devtools`
/// mixin — if the resolver accidentally shipped apt-get to fedora, the
/// setup phase would fail.
///
/// To run:
///   cargo test `fedora_base_boots_and_provisions` -- --include-ignored --nocapture
#[tokio::test]
#[ignore = "downloads a real cloud image and boots a VM — slow"]
#[serial(vm_boot)]
async fn fedora_base_boots_and_provisions() {
    if !qemu_img_available() || !iso_tool_available() || !qemu_available() {
        eprintln!("required tools not installed — skipping fedora_base_boots_and_provisions");
        return;
    }

    dirs::ensure_dirs().await.unwrap();

    let name = "_test-fedora";
    cleanup(name).await;

    // Inject a marker file to confirm file copy works on a non-debian guest.
    let tmp_dir = tempfile::tempdir().unwrap();
    let test_file = tmp_dir.path().join("agv-test-inject.txt");
    tokio::fs::write(&test_file, "injected-by-agv-on-fedora")
        .await
        .unwrap();

    let config = config::resolve(config::Config {
        base: Some(config::BaseConfig {
            from: Some("fedora-43".to_string()),
            include: vec!["devtools".to_string()],
            ..Default::default()
        }),
        vm: Some(config::VmConfig {
            memory: Some("2G".to_string()),
            cpus: Some(2),
            disk: Some("10G".to_string()),
        }),
        files: vec![config::FileEntry {
            source: test_file.to_str().unwrap().to_string(),
            dest: "/home/agent/.config/agv-test/agv-test-inject.txt".to_string(),
            optional: false,
        }],
        setup: vec![],
        provision: vec![config::ProvisionStep {
            source: None,
            run: Some(
                "cat /home/agent/.config/agv-test/agv-test-inject.txt && command -v dnf >/dev/null && echo dnf-present".to_string(),
            ),
            script: None,
        }],
        forwards: vec![],
        os_families: None,
        supports: None,
    auto_forwards: None,
    notes: vec![],
    manual_steps: vec![],
    })
    .unwrap();

    // Sanity: the resolver should have inherited the fedora family and
    // pulled in the dnf setup step (not the apt one).
    assert_eq!(config.os_family, "fedora");
    let dnf_step_present = config
        .setup
        .iter()
        .filter_map(|s| s.run.as_deref())
        .any(|cmd| cmd.starts_with("dnf install"));
    assert!(
        dnf_step_present,
        "expected devtools' fedora setup step; got: {:?}",
        config
            .setup
            .iter()
            .filter_map(|s| s.run.clone())
            .collect::<Vec<_>>()
    );

    vm::create(name, &config, true, false, false, true)
        .await
        .unwrap();

    let inst_dir = dirs::instance_dir(name).unwrap();
    let inst = Instance {
        name: name.to_string(),
        dir: inst_dir,
    };

    let status = inst.read_status().await.unwrap();
    assert_eq!(status, Status::Running);
    assert!(inst.pid_path().exists());
    assert!(inst.ssh_port_path().exists());

    // The provision step cats the injected file and checks dnf is on PATH.
    let log = tokio::fs::read_to_string(inst.provision_log_path())
        .await
        .unwrap();
    assert!(
        log.contains("injected-by-agv-on-fedora"),
        "provision.log should contain injected file content on fedora:\n{log}"
    );
    assert!(
        log.contains("dnf-present"),
        "provision.log should confirm dnf is installed on fedora:\n{log}"
    );

    assert!(inst.is_provisioned(), "instance should be marked provisioned");

    cleanup(name).await;
}

/// End-to-end test for `[auto_forwards]`: a mixin declares a named
/// forward plus a systemd user service listening on that guest port,
/// agv allocates a host port at start, and an HTTP request from the
/// host through the SSH tunnel reaches the service inside the guest.
///
/// This is the smallest test that would have caught the
/// `create_inner`-skipping-apply-forwards bug — the forwards supervisor
/// has to actually come up during `agv create --start`, not just on
/// subsequent `agv start` invocations.
///
/// Uses `python3 -m http.server` as the guest-side service (stdlib on
/// debian, no extra packages) instead of a full `gui-xfce` stack, so
/// the test cost stays in the same league as the existing boot tests.
///
/// To run:
///   cargo test `auto_forwards_end_to_end` -- --include-ignored --nocapture
#[tokio::test]
#[ignore = "downloads a real cloud image and boots a VM — slow"]
#[serial(vm_boot)]
async fn auto_forwards_end_to_end() {
    if !qemu_img_available() || !iso_tool_available() || !qemu_available() {
        eprintln!("required tools not installed — skipping auto_forwards_end_to_end");
        return;
    }

    dirs::ensure_dirs().await.unwrap();

    // Drop a test mixin into the user images dir. It declares a named
    // auto_forward and starts python's http.server on the guest port.
    let images_dir = dirs::images_dir().unwrap();
    tokio::fs::create_dir_all(&images_dir).await.unwrap();
    let mixin_name = "_agv-test-autofwd";
    let mixin_path = images_dir.join(format!("{mixin_name}.toml"));
    // System service rather than user service — the latter's
    // `loginctl enable-linger` + mid-session `systemctl --user enable --now`
    // is a known-flaky pattern that can race with pam_systemd on first
    // provisioning. System services start reliably via a single
    // `systemctl enable --now`.
    let mixin_contents = r#"
[auto_forwards.httptest]
guest_port = 9001

[[provision]]
run = """
set -eu
sudo tee /etc/systemd/system/agv-test-http.service >/dev/null <<'UNIT'
[Unit]
Description=agv auto_forwards end-to-end test HTTP server
After=network-online.target

[Service]
Type=simple
ExecStart=/usr/bin/python3 -m http.server --bind 127.0.0.1 9001
Restart=on-failure

[Install]
WantedBy=multi-user.target
UNIT
sudo systemctl daemon-reload
sudo systemctl enable --now agv-test-http
"""
"#;
    tokio::fs::write(&mixin_path, mixin_contents).await.unwrap();

    let name = "_test-auto-forwards";
    cleanup(name).await;

    // The forward supervisor re-execs the agv binary via `__forward-daemon`.
    // Inside `cargo test` `current_exe()` returns the test binary (libtest),
    // which silently treats those args as a test filter and exits — no
    // supervisor, no error. Point the spawner at the real agv binary.
    agv::vm::forwarding::set_agv_binary_for_tests(std::path::Path::new(env!(
        "CARGO_BIN_EXE_agv"
    )));

    let config = config::resolve(config::Config {
        base: Some(config::BaseConfig {
            from: Some("debian-12".to_string()),
            include: vec![mixin_name.to_string()],
            ..Default::default()
        }),
        vm: Some(config::VmConfig {
            memory: Some("1G".to_string()),
            cpus: Some(2),
            disk: Some("10G".to_string()),
        }),
        ..Default::default()
    })
    .unwrap();

    vm::create(name, &config, true, false, false, true).await.unwrap();

    let inst_dir = dirs::instance_dir(name).unwrap();
    let inst = Instance {
        name: name.to_string(),
        dir: inst_dir,
    };

    assert_eq!(inst.read_status().await.unwrap(), Status::Running);

    // The auto_forward plumbing should have allocated a host port and
    // written it to <instance>/httptest_port.
    let port_path = inst.auto_forward_port_path("httptest");
    assert!(
        port_path.exists(),
        "expected auto_forward port file at {}",
        port_path.display()
    );
    let port: u16 = tokio::fs::read_to_string(&port_path)
        .await
        .unwrap()
        .trim()
        .parse()
        .expect("port file should contain a valid u16");
    assert!(port > 0);

    // Hit the python http.server inside the guest via the SSH tunnel.
    // Retry for up to 30s because the guest's systemd user service
    // may take a moment to come up after first boot.
    let url = format!("http://127.0.0.1:{port}/");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .unwrap();
    let mut body = None;
    for _ in 0..30 {
        if let Ok(resp) = client.get(&url).send().await {
            if resp.status().is_success() {
                body = Some(resp.text().await.unwrap_or_default());
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    let Some(body) = body else {
        // Collect diagnostics from both sides so a regression is actionable.
        let service_status = ssh::run_cmd(
            &inst,
            &config.user,
            &["systemctl status agv-test-http --no-pager -l || true".to_string()],
        )
        .await
        .unwrap_or_else(|e| format!("<ssh failed: {e:#}>"));
        let journal = ssh::run_cmd(
            &inst,
            &config.user,
            &["sudo journalctl -u agv-test-http --no-pager -n 50 || true".to_string()],
        )
        .await
        .unwrap_or_else(|e| format!("<ssh failed: {e:#}>"));
        let forwards_toml = tokio::fs::read_to_string(inst.forwards_path())
            .await
            .unwrap_or_else(|e| format!("<read failed: {e}>"));

        panic!(
            "never got a 200 OK from the tunneled python http.server.\n\
             \n\
             host port (from {path:?}): {port}\n\
             \n\
             ---- <instance>/forwards.toml ----\n{forwards_toml}\n\
             ---- guest: systemctl status agv-test-http ----\n{service_status}\n\
             ---- guest: journalctl -u agv-test-http ----\n{journal}",
            path = port_path.display(),
        );
    };
    assert!(
        body.contains("Directory listing") || body.contains("<title>"),
        "expected python http.server index page, got: {body}"
    );

    cleanup(name).await;
    let _ = tokio::fs::remove_file(&mixin_path).await;
}

/// Verify that suspend saves VM state and resume restores it.
///
/// Creates a marker file in /run (tmpfs/RAM-backed), suspends the VM, resumes
/// it, then checks that:
///   1. The marker file is still there (RAM state preserved)
///   2. /proc/uptime is at least as large as it was before suspend (continuous,
///      not reset by a reboot)
#[tokio::test]
#[ignore = "downloads a real cloud image and boots a VM — slow"]
#[serial(vm_boot)]
async fn suspend_and_resume_preserves_state() {
    if !qemu_img_available() || !iso_tool_available() || !qemu_available() {
        eprintln!("required tools not installed — skipping suspend_and_resume_preserves_state");
        return;
    }

    dirs::ensure_dirs().await.unwrap();

    let name = "_test-suspend-resume";
    cleanup(name).await;

    let config = config::resolve(config::Config {
        base: Some(config::BaseConfig {
            from: Some("debian-12".to_string()),
            ..Default::default()
        }),
        vm: Some(config::VmConfig {
            memory: Some("1G".to_string()),
            cpus: Some(2),
            disk: Some("10G".to_string()),
        }),
        files: vec![],
        setup: vec![],
        provision: vec![],
        forwards: vec![],
    os_families: None,
    supports: None,
    auto_forwards: None,
    notes: vec![],
    manual_steps: vec![],
    })
    .unwrap();

    vm::create(name, &config, true, false, false, true).await.unwrap();

    let inst_dir = dirs::instance_dir(name).unwrap();
    let inst = Instance {
        name: name.to_string(),
        dir: inst_dir,
    };

    // Status should be running.
    assert_eq!(inst.read_status().await.unwrap(), Status::Running);

    // Write a marker file to /run (tmpfs, lives only in RAM) and capture
    // the uptime before suspending.
    let marker = format!("agv-suspend-test-{}", std::process::id());
    ssh::run_cmd(
        &inst,
        &config.user,
        &[format!("sudo sh -c 'echo {marker} > /run/agv-marker'")],
    )
    .await
    .expect("failed to write marker");

    let uptime_before_raw = ssh::run_cmd(
        &inst,
        &config.user,
        &["cat /proc/uptime".to_string()],
    )
    .await
    .unwrap();
    let uptime_before: f64 = uptime_before_raw
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();

    // Suspend the VM.
    vm::suspend(name).await.expect("suspend failed");
    assert_eq!(inst.read_status().await.unwrap(), Status::Suspended);
    // QEMU process should be gone.
    assert!(!inst.pid_path().exists(), "PID file should be removed after suspend");
    assert!(!inst.ssh_port_path().exists(), "ssh_port should be removed after suspend");

    // Resume the VM.
    vm::resume(name, false, true).await.expect("resume failed");
    assert_eq!(inst.read_status().await.unwrap(), Status::Running);
    assert!(inst.pid_path().exists(), "PID file should exist after resume");

    // The marker file in tmpfs should still be there — proves RAM state was
    // saved and restored.
    let marker_content = ssh::run_cmd(
        &inst,
        &config.user,
        &["cat /run/agv-marker".to_string()],
    )
    .await
    .expect("failed to read marker after resume");
    assert!(
        marker_content.contains(&marker),
        "marker file lost after suspend/resume — state was not preserved (got: {marker_content:?})"
    );

    // Uptime should be at least as large as before — proves the VM did not
    // reboot during suspend/resume.
    let uptime_after_raw = ssh::run_cmd(
        &inst,
        &config.user,
        &["cat /proc/uptime".to_string()],
    )
    .await
    .unwrap();
    let uptime_after: f64 = uptime_after_raw
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        uptime_after >= uptime_before,
        "VM uptime decreased ({uptime_before} → {uptime_after}) — VM rebooted instead of resuming"
    );

    cleanup(name).await;
}

/// Verify that a failing provision step puts the VM into a broken state
/// with `provision_state` pointing at the failed step, and that
/// `agv start --retry` resumes from that step (skipping completed ones).
#[tokio::test]
#[ignore = "downloads a real cloud image and boots a VM — slow"]
#[serial(vm_boot)]
async fn provision_failure_then_retry_resumes() {
    if !qemu_img_available() || !iso_tool_available() || !qemu_available() {
        eprintln!("required tools not installed — skipping provision_failure_then_retry_resumes");
        return;
    }

    dirs::ensure_dirs().await.unwrap();

    let name = "_test-retry";
    cleanup(name).await;

    // Three provision steps:
    //   0. echo "first" >> /tmp/agv-retry-log    (always succeeds)
    //   1. fails on the first run, succeeds on the second (counter file)
    //   2. echo "third" >> /tmp/agv-retry-log    (only runs after retry)
    //
    // After the initial create:
    //   - Step 0 ran → log contains "first"
    //   - Step 1 failed → broken, provision_state.index = 1
    // After retry:
    //   - Step 1 ran successfully → counter file proves it
    //   - Step 2 ran → log contains "third"
    //   - Step 0 should NOT have run again (we'd see "first" twice)
    let config = config::resolve(config::Config {
        base: Some(config::BaseConfig {
            from: Some("debian-12".to_string()),
            ..Default::default()
        }),
        vm: Some(config::VmConfig {
            memory: Some("1G".to_string()),
            cpus: Some(2),
            disk: Some("10G".to_string()),
        }),
        files: vec![],
        setup: vec![],
        provision: vec![
            config::ProvisionStep {
                source: None,
                run: Some("echo first >> /tmp/agv-retry-log".to_string()),
                script: None,
            },
            config::ProvisionStep {
                source: None,
                run: Some(
                    "if [ -f /tmp/agv-retry-counter ]; then \
                       echo second >> /tmp/agv-retry-log; \
                     else \
                       touch /tmp/agv-retry-counter; \
                       exit 1; \
                     fi".to_string(),
                ),
                script: None,
            },
            config::ProvisionStep {
                source: None,
                run: Some("echo third >> /tmp/agv-retry-log".to_string()),
                script: None,
            },
        ],
        forwards: vec![],
    os_families: None,
    supports: None,
    auto_forwards: None,
    notes: vec![],
    manual_steps: vec![],
    })
    .unwrap();

    // First create — expected to fail at step 1.
    let create_result = vm::create(name, &config, true, false, false, true).await;
    assert!(
        create_result.is_err(),
        "expected create to fail because of the deliberately failing provision step"
    );

    let inst_dir = dirs::instance_dir(name).unwrap();
    let inst = Instance {
        name: name.to_string(),
        dir: inst_dir,
    };

    // VM should be marked broken with provision_state pointing at step 1.
    assert_eq!(inst.read_status().await.unwrap(), Status::Broken);
    let state = inst.read_provision_state().await;
    assert_eq!(state.phase, Phase::Provision, "expected to be in provision phase");
    assert_eq!(state.index, 1, "expected to have failed at step index 1");
    assert!(state.error.is_some(), "expected an error message in state");

    // QEMU should still be running (we leave it for debugging).
    assert!(inst.is_process_alive().await, "QEMU should still be alive after broken first-boot");

    // The first step should have run exactly once.
    let log_after_fail = ssh::run_cmd(
        &inst,
        &config.user,
        &["cat /tmp/agv-retry-log".to_string()],
    )
    .await
    .expect("failed to read retry log via SSH");
    assert_eq!(
        log_after_fail.matches("first").count(),
        1,
        "expected step 0 to have run once before failure (got: {log_after_fail:?})"
    );
    assert!(
        !log_after_fail.contains("third"),
        "step 2 should not have run yet (got: {log_after_fail:?})"
    );

    // Retry — should resume from step 1.
    vm::start(name, true, false, false, true).await.expect("retry failed");

    assert_eq!(inst.read_status().await.unwrap(), Status::Running);
    assert!(inst.is_provisioned(), "VM should now be fully provisioned");

    // Check the log: should contain first (once), second, third.
    let log_after_retry = ssh::run_cmd(
        &inst,
        &config.user,
        &["cat /tmp/agv-retry-log".to_string()],
    )
    .await
    .expect("failed to read retry log via SSH after retry");
    assert_eq!(
        log_after_retry.matches("first").count(),
        1,
        "step 0 should not have run again on retry (got: {log_after_retry:?})"
    );
    assert!(
        log_after_retry.contains("second"),
        "step 1 should have run on retry (got: {log_after_retry:?})"
    );
    assert!(
        log_after_retry.contains("third"),
        "step 2 should have run after step 1 succeeded (got: {log_after_retry:?})"
    );

    cleanup(name).await;
}
