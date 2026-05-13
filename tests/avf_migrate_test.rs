//! End-to-end test for `agv backend migrate-to-avf` — the path
//! users hit when moving an existing QEMU VM onto the AVF backend.
//! Verifies the full sequence: create a QEMU VM, stop it, migrate,
//! start under AVF, SSH still works.
//!
//! Runtime-skip:
//! - Non-macOS: compiled out (`#![cfg(target_os = "macos")]`).
//! - Missing the Swift runner: prints a skip message.
//! - Missing QEMU tools: skip — the create step uses QEMU as the
//!   source backend.
//! - Missing the cached Debian image: agv downloads it on the
//!   first run (~330 MB).
//!
//! Marked `#[ignore]` because it boots two real VMs back-to-back
//! and may download a cloud image. Run with
//! `cargo test -- --include-ignored --nocapture`.

#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};

use serial_test::serial;

fn runner_binary() -> Option<PathBuf> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let path =
        PathBuf::from(manifest_dir).join("swift/avf-runner/.build/release/agv-avf-runner");
    if path.exists() { Some(path) } else { None }
}

fn qemu_img_available() -> bool {
    std::process::Command::new("qemu-img")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn iso_tool_available() -> bool {
    // macOS has hdiutil built in; this is a no-op on macOS but kept
    // here so the skip message in the test matches the QEMU slow
    // tests' shape — and to make the test usable on Linux later if
    // we ever extend it.
    true
}

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

/// Symlink the Swift runner alongside the `agv` binary so the
/// production resolver finds it. See the comment in
/// `tests/avf_e2e_test.rs` for why we symlink instead of copy.
fn ensure_runner_alongside_agv(source: &Path) {
    let agv_bin = PathBuf::from(env!("CARGO_BIN_EXE_agv"));
    let dest = agv_bin
        .parent()
        .expect("agv binary has a parent dir")
        .join("agv-avf-runner");
    if let Ok(cur) = std::fs::read_link(&dest) {
        if cur == source {
            return;
        }
    }
    let _ = std::fs::remove_file(&dest);
    std::os::unix::fs::symlink(source, &dest)
        .expect("symlink runner alongside agv binary");
}

fn agv(data_dir: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_agv"));
    cmd.env("AGV_DATA_DIR", data_dir).arg("--quiet");
    cmd
}

async fn write_config(host_dir: &Path, body: &str) -> PathBuf {
    let path = host_dir.join("agv.toml");
    tokio::fs::write(&path, body).await.unwrap();
    path
}

fn parse_json(label: &str, stdout: &[u8]) -> serde_json::Value {
    let s = String::from_utf8(stdout.to_vec()).unwrap();
    serde_json::from_str(s.trim()).unwrap_or_else(|e| {
        panic!("{label} stdout didn't parse as JSON: {e}\nstdout:\n{s}")
    })
}

async fn inspect(data_dir: &Path, name: &str) -> serde_json::Value {
    let output = agv(data_dir)
        .args(["inspect", "--json", name])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "agv inspect --json {name} failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    parse_json(&format!("agv inspect {name}"), &output.stdout)
}

async fn destroy(data_dir: &Path, name: &str) {
    let _ = agv(data_dir)
        .args(["destroy", "--force", name])
        .output()
        .await;
}

/// Full QEMU→AVF migration path through the CLI:
///   1. `agv create --start --backend qemu` (implicit default)
///   2. SSH works on the QEMU-hosted VM.
///   3. `agv stop` — required before migration.
///   4. `agv backend migrate-to-avf --json --delete-qcow2`.
///   5. `agv start` — boots under AVF.
///   6. SSH works on the AVF-hosted VM.
///   7. `agv inspect` reports `backend=avf` and the original
///      `disk.qcow2` is gone (we passed `--delete-qcow2`).
#[tokio::test]
#[ignore = "boots a real QEMU VM, migrates to AVF, boots under AVF — slow, ~120s"]
#[serial]
async fn migrate_qemu_vm_to_avf_backend() {
    let Some(runner) = runner_binary() else {
        eprintln!("agv-avf-runner not built — skipping migrate_qemu_vm_to_avf_backend");
        return;
    };
    if !qemu_available() || !qemu_img_available() || !iso_tool_available() {
        eprintln!("QEMU tools missing — skipping migrate_qemu_vm_to_avf_backend");
        return;
    }
    ensure_runner_alongside_agv(&runner);

    let data_dir = tempfile::tempdir().unwrap();
    let host_tmp = tempfile::tempdir().unwrap();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let suffix = format!("{:06x}", (ts as u64) & 0xff_ffff);
    let name = format!("avf-mig-{suffix}");
    let name = name.as_str();

    // Cold-boot on the QEMU backend (the default — no `backend = ...`).
    let qemu_config = r#"
[base]
from = "debian-12"

[vm]
memory = "1G"
cpus = 2
disk = "3G"
"#;
    let toml_path = write_config(host_tmp.path(), qemu_config).await;

    // --- 1. create QEMU VM ---
    let create_output = agv(data_dir.path())
        .args([
            "create",
            "--start",
            "--json",
            "--config",
            toml_path.to_str().unwrap(),
            name,
        ])
        .output()
        .await
        .unwrap();
    if !create_output.status.success() {
        destroy(data_dir.path(), name).await;
        panic!(
            "agv create (qemu) failed: {}",
            String::from_utf8_lossy(&create_output.stderr),
        );
    }
    let report = parse_json("agv create", &create_output.stdout);
    assert_eq!(report["status"], "running");
    assert_eq!(report["backend"], "qemu");

    let inst_dir = PathBuf::from(report["data_dir"].as_str().unwrap());
    assert!(inst_dir.join("disk.qcow2").exists());
    assert!(!inst_dir.join("disk.raw").exists());

    // --- 2. SSH works on QEMU side ---
    let qemu_ssh = agv(data_dir.path())
        .args(["ssh", name, "--", "whoami"])
        .output()
        .await
        .unwrap();
    if !qemu_ssh.status.success() {
        destroy(data_dir.path(), name).await;
        panic!(
            "agv ssh (qemu) failed: {}",
            String::from_utf8_lossy(&qemu_ssh.stderr),
        );
    }
    let who = String::from_utf8_lossy(&qemu_ssh.stdout);
    assert!(who.trim() == "agent", "QEMU SSH should run as agent, got: {who:?}");

    // --- 3. stop ---
    let stop_output = agv(data_dir.path())
        .args(["stop", name])
        .output()
        .await
        .unwrap();
    if !stop_output.status.success() {
        destroy(data_dir.path(), name).await;
        panic!(
            "agv stop failed: {}",
            String::from_utf8_lossy(&stop_output.stderr),
        );
    }
    let stopped = inspect(data_dir.path(), name).await;
    assert_eq!(stopped["status"], "stopped");
    assert_eq!(stopped["backend"], "qemu");

    // --- 4. migrate to AVF ---
    let mig_output = agv(data_dir.path())
        .args([
            "backend",
            "migrate-to-avf",
            "--json",
            "--delete-qcow2",
            name,
        ])
        .output()
        .await
        .unwrap();
    if !mig_output.status.success() {
        destroy(data_dir.path(), name).await;
        panic!(
            "agv backend migrate-to-avf failed: {}\nstdout: {}",
            String::from_utf8_lossy(&mig_output.stderr),
            String::from_utf8_lossy(&mig_output.stdout),
        );
    }
    let mig_report = parse_json("migrate", &mig_output.stdout);
    assert_eq!(mig_report["name"], name);
    assert_eq!(mig_report["qcow2_disk_kept"], false);
    assert!(
        mig_report["raw_disk_size_bytes"].as_u64().unwrap() > 1024 * 1024,
        "raw disk size implausibly small: {:?}",
        mig_report["raw_disk_size_bytes"],
    );

    // qcow2 is gone, raw is present.
    assert!(
        !inst_dir.join("disk.qcow2").exists(),
        "disk.qcow2 should be deleted after --delete-qcow2"
    );
    assert!(
        inst_dir.join("disk.raw").exists(),
        "disk.raw should exist after migration"
    );

    let after_mig = inspect(data_dir.path(), name).await;
    assert_eq!(
        after_mig["backend"], "avf",
        "inspect should report backend=avf after migration"
    );
    assert_eq!(after_mig["status"], "stopped");

    // --- 5. start under AVF ---
    let start_output = agv(data_dir.path())
        .args(["start", name])
        .output()
        .await
        .unwrap();
    if !start_output.status.success() {
        let runner_log = std::fs::read_to_string(inst_dir.join("avf-runner.log"))
            .unwrap_or_default();
        // Try a status RPC against the runner to see if the VM is
        // actually running and just unreachable.
        use std::io::{Read, Write};
        use std::os::unix::net::UnixStream;
        let sock = inst_dir.join("avf-control.sock");
        let mut runner_status = String::new();
        if let Ok(mut s) = UnixStream::connect(&sock) {
            let _ = s.write_all(b"{\"op\":\"status\"}\n");
            let _ = s.read_to_string(&mut runner_status);
        }
        destroy(data_dir.path(), name).await;
        panic!(
            "agv start (avf) failed: {}\n--- runner log ---\n{runner_log}\n--- runner status RPC ---\n{runner_status}",
            String::from_utf8_lossy(&start_output.stderr),
        );
    }

    // --- 6. SSH on AVF side ---
    let avf_ssh = agv(data_dir.path())
        .args(["ssh", name, "--", "whoami"])
        .output()
        .await
        .unwrap();
    if !avf_ssh.status.success() {
        destroy(data_dir.path(), name).await;
        panic!(
            "agv ssh (avf, post-migration) failed: {}",
            String::from_utf8_lossy(&avf_ssh.stderr),
        );
    }
    let who2 = String::from_utf8_lossy(&avf_ssh.stdout);
    assert!(
        who2.trim() == "agent",
        "AVF SSH should run as agent, got: {who2:?}"
    );

    destroy(data_dir.path(), name).await;
}

/// Refuses to migrate a running VM — the user has to stop first.
/// Runtime-skip if AVF runner / QEMU aren't available.
#[tokio::test]
#[ignore = "boots a real QEMU VM — slow, ~30s"]
#[serial]
async fn migrate_refuses_running_vm() {
    let Some(runner) = runner_binary() else {
        eprintln!("agv-avf-runner not built — skipping migrate_refuses_running_vm");
        return;
    };
    if !qemu_available() || !qemu_img_available() {
        eprintln!("QEMU tools missing — skipping migrate_refuses_running_vm");
        return;
    }
    ensure_runner_alongside_agv(&runner);

    let data_dir = tempfile::tempdir().unwrap();
    let host_tmp = tempfile::tempdir().unwrap();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let suffix = format!("{:06x}", (ts as u64) & 0xff_ffff);
    let name = format!("avf-mig-run-{suffix}");
    let name = name.as_str();

    let qemu_config = r#"
[base]
from = "debian-12"

[vm]
memory = "1G"
cpus = 2
disk = "3G"
"#;
    let toml_path = write_config(host_tmp.path(), qemu_config).await;

    // Create + start a QEMU VM, leave it running.
    let create = agv(data_dir.path())
        .args([
            "create",
            "--start",
            "--json",
            "--config",
            toml_path.to_str().unwrap(),
            name,
        ])
        .output()
        .await
        .unwrap();
    if !create.status.success() {
        destroy(data_dir.path(), name).await;
        panic!(
            "agv create failed: {}",
            String::from_utf8_lossy(&create.stderr),
        );
    }

    let mig = agv(data_dir.path())
        .args(["backend", "migrate-to-avf", name])
        .output()
        .await
        .unwrap();
    let stderr = String::from_utf8_lossy(&mig.stderr);
    assert!(
        !mig.status.success(),
        "migrate of a running VM should fail; stderr was: {stderr}"
    );
    // The Error::VmBadState shape mentions the current and expected
    // statuses; sanity-check that "running" and "stopped" both appear.
    assert!(
        stderr.contains("running") && stderr.contains("stopped"),
        "stderr should mention current vs expected status: {stderr}"
    );

    destroy(data_dir.path(), name).await;
}
