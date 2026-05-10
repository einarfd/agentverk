//! Integration tests for the agv-avf-runner Swift binary (macOS only).
//!
//! Runtime-skip:
//! - On non-macOS hosts, every test is a no-op (the runner doesn't exist
//!   on those platforms — it's the Apple Virtualization wrapper).
//! - On macOS, tests skip if the runner binary hasn't been built. Run
//!   `just build-avf-runner` first, or these tests will be no-ops.
//!
//! Tests drive the binary as a subprocess and observe stdout/exit codes
//! — the same contract the Rust agv binary will use to control the
//! runner in production. This catches the things most likely to break
//! across builds: ad-hoc codesigning (without it, AVF API calls fail
//! with `VZErrorDomain Code=2`), entitlement preservation through
//! `swift build`, and the JSON config protocol.

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::process::Command;

/// Locate the release build of the runner relative to the workspace
/// root. Returns `None` if it hasn't been built yet — tests should skip
/// in that case.
fn runner_binary() -> Option<PathBuf> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let path = PathBuf::from(manifest_dir).join("swift/avf-runner/.build/release/agv-avf-runner");
    if path.exists() {
        Some(path)
    } else {
        None
    }
}

/// Generate a tiny seed.iso via macOS's built-in `hdiutil`, matching
/// what the real lifecycle code does. Returns the iso path inside `dir`.
fn make_seed_iso(dir: &std::path::Path) -> PathBuf {
    let seed_src = dir.join("seed-src");
    std::fs::create_dir_all(&seed_src).unwrap();
    std::fs::write(
        seed_src.join("meta-data"),
        "instance-id: avf-test\nlocal-hostname: avf-test\n",
    )
    .unwrap();
    std::fs::write(seed_src.join("user-data"), "#cloud-config\n").unwrap();

    let iso = dir.join("seed.iso");
    let status = Command::new("hdiutil")
        .args([
            "makehybrid",
            "-o",
            iso.to_str().unwrap(),
            "-hfs",
            "-joliet",
            "-iso",
            "-default-volume-name",
            "cidata",
            seed_src.to_str().unwrap(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("hdiutil failed to spawn");
    assert!(status.success(), "hdiutil makehybrid failed");
    iso
}

/// Generate a small empty raw disk file (32 MiB sparse).
fn make_empty_raw(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("disk.raw");
    let f = std::fs::File::create(&path).unwrap();
    // 32 MiB sparse file — enough for AVF to accept the attachment;
    // not actually bootable, but the validate path doesn't boot.
    f.set_len(32 * 1024 * 1024).unwrap();
    path
}

/// Write a runner config JSON pointing at the supplied disk + seed paths.
fn write_config(
    dir: &std::path::Path,
    name: &str,
    disk: &std::path::Path,
    seed: &std::path::Path,
) -> PathBuf {
    let cfg = format!(
        r#"{{
  "name": "{name}",
  "memory_bytes": 1073741824,
  "cpu_count": 2,
  "disk_path": "{disk}",
  "seed_iso_path": "{seed}",
  "efi_variable_store_path": "{efi}",
  "serial_log_path": "{serial}",
  "control_socket_path": "{ctl}"
}}
"#,
        name = name,
        disk = disk.display(),
        seed = seed.display(),
        efi = dir.join("efi-vars.bin").display(),
        serial = dir.join("serial.log").display(),
        ctl = dir.join("control.sock").display(),
    );
    let path = dir.join("config.json");
    std::fs::write(&path, cfg).unwrap();
    path
}

#[test]
fn version_flag_succeeds() {
    let Some(binary) = runner_binary() else {
        eprintln!(
            "agv-avf-runner not built — skipping version_flag_succeeds (run: just build-avf-runner)"
        );
        return;
    };
    let out = Command::new(&binary).arg("--version").output().unwrap();
    assert!(out.status.success(), "--version exited non-zero");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with("agv-avf-runner "),
        "unexpected --version output: {stdout}"
    );
}

#[test]
fn unknown_arg_exits_with_usage() {
    let Some(binary) = runner_binary() else {
        eprintln!("agv-avf-runner not built — skipping unknown_arg_exits_with_usage");
        return;
    };
    let out = Command::new(&binary).arg("--bogus").output().unwrap();
    assert!(!out.status.success(), "unknown arg should exit non-zero");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unrecognized argument"),
        "expected 'unrecognized argument' in stderr, got: {stderr}"
    );
}

#[test]
fn config_without_required_path_fails() {
    let Some(binary) = runner_binary() else {
        eprintln!("agv-avf-runner not built — skipping config_without_required_path_fails");
        return;
    };
    let out = Command::new(&binary).arg("--config").output().unwrap();
    assert!(
        !out.status.success(),
        "--config without value should exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("requires a path"),
        "expected helpful error in stderr, got: {stderr}"
    );
}

#[test]
fn validate_succeeds_for_well_formed_config() {
    let Some(binary) = runner_binary() else {
        eprintln!(
            "agv-avf-runner not built — skipping validate_succeeds_for_well_formed_config"
        );
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let disk = make_empty_raw(dir.path());
    let seed = make_seed_iso(dir.path());
    let cfg = write_config(dir.path(), "avf-test", &disk, &seed);

    let out = Command::new(&binary)
        .arg("--config")
        .arg(&cfg)
        .arg("--validate-only")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "validate should succeed for a well-formed config\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("config validates"),
        "expected 'config validates' confirmation in stdout: {stdout}"
    );
    // Side-effect: AVF lazily creates the EFI variable store on first use.
    assert!(
        dir.path().join("efi-vars.bin").exists(),
        "EFI variable store should have been created"
    );
}

#[test]
fn validate_fails_when_disk_missing() {
    let Some(binary) = runner_binary() else {
        eprintln!("agv-avf-runner not built — skipping validate_fails_when_disk_missing");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let seed = make_seed_iso(dir.path());
    let bogus_disk = dir.path().join("nonexistent.raw");
    let cfg = write_config(dir.path(), "avf-test", &bogus_disk, &seed);

    let out = Command::new(&binary)
        .arg("--config")
        .arg(&cfg)
        .arg("--validate-only")
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "validate should fail when disk path doesn't exist"
    );
}

/// Slow boot test: spin up a real AVF VM from the cached Debian raw,
/// wait for the kernel to log to the serial pipe, then SIGTERM and
/// confirm graceful exit. Skipped unless the cached raw exists in the
/// PoC location (`/tmp/qcow2-poc/out/...`); reproduce by running the
/// PoC converter once.
///
/// Marked `#[ignore]` because it boots a real VM (~5–10s), which is
/// the same cost as our slow QEMU boot tests.
#[test]
#[ignore = "boots a real Apple Virtualization VM — slow"]
fn boot_and_sigterm_exits_cleanly() {
    let Some(binary) = runner_binary() else {
        eprintln!("agv-avf-runner not built — skipping boot_and_sigterm_exits_cleanly");
        return;
    };
    let cached_raw = PathBuf::from(
        "/tmp/qcow2-poc/out/debian-12-genericcloud-arm64-20260210-2384.ours.raw",
    );
    if !cached_raw.exists() {
        eprintln!(
            "{} not present — skipping boot_and_sigterm_exits_cleanly (run the qcow2-rs PoC first to populate it)",
            cached_raw.display()
        );
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("disk.raw");
    std::fs::copy(&cached_raw, &disk).unwrap();
    let seed = make_seed_iso(dir.path());
    let cfg = write_config(dir.path(), "avf-boot-test", &disk, &seed);

    let mut child = Command::new(&binary)
        .arg("--config")
        .arg(&cfg)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    // Let the VM actually boot before we signal it. AVF is fast — 8s is
    // plenty to clear UEFI handoff and have the kernel running.
    std::thread::sleep(std::time::Duration::from_secs(8));

    // We can't assert on serial.log content here: AVF only exposes
    // virtio-console (`/dev/hvc0`), but the Debian cloud kernel is
    // built with `console=ttyAMA0` baked into its GRUB config. Under
    // QEMU that's the PL011 UART (works); under AVF that device
    // doesn't exist, so the kernel logs go to /dev/null from our
    // perspective. Wiring `console=hvc0` requires modifying the disk
    // image's GRUB config at create time or shipping our own
    // bootloader; tracked for a follow-up commit.
    //
    // What we *can* assert: the runner is still alive after 8s. If
    // VZ.start() had failed, the runner would have exited immediately
    // with code 1.
    if let Some(status) = child.try_wait().expect("try_wait failed") {
        panic!(
            "agv-avf-runner exited unexpectedly during boot ({status:?}); \
             VZ.start() likely failed"
        );
    }

    // SIGTERM should trigger the runner's requestStop path. The kernel
    // ACPIs through systemd shutdown, then the VM stops, then the
    // process exits. Allow a generous window for the guest to react.
    let pid = child.id();
    let kill = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("kill -TERM failed to spawn");
    assert!(kill.success(), "kill -TERM failed");

    let mut exited = false;
    let mut exit_status = None;
    for _ in 0..60 {
        match child.try_wait() {
            Ok(Some(s)) => {
                exited = true;
                exit_status = Some(s);
                break;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_secs(1)),
            Err(e) => panic!("try_wait failed: {e}"),
        }
    }
    if !exited {
        let _ = child.kill();
        let _ = child.wait();
        panic!("agv-avf-runner did not exit within 60s after SIGTERM");
    }
    let status = exit_status.unwrap();
    assert!(
        status.success(),
        "agv-avf-runner should exit cleanly after SIGTERM, got {status:?}"
    );
}
