//! End-to-end integration test for the AVF backend through the
//! `agv` CLI — the same path real users hit. Boots a Debian VM under
//! Apple Virtualization, asserts SSH works, exercises suspend/resume,
//! then destroys. This is the load-bearing test for `agv create
//! --backend avf` shippability.
//!
//! Runtime-skip:
//! - On non-macOS hosts: the whole file is compiled out via
//!   `#![cfg(target_os = "macos")]`.
//! - On macOS without the Swift runner built: prints a skip message
//!   and exits. Build with `just build-avf-runner`.
//! - On macOS without the cached Debian image: agv downloads it
//!   on the first run (~330 MB); subsequent runs reuse the cache
//!   under `AGV_DATA_DIR/cache/images/`.
//!
//! Marked `#[ignore]` because it boots a real VM and may download a
//! cloud image. Run with `cargo test -- --include-ignored --nocapture`.

#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};

use serial_test::serial;

/// Locate the release build of the Swift runner. Returns None if it
/// hasn't been built — the e2e test then skips.
fn runner_binary() -> Option<PathBuf> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let path =
        PathBuf::from(manifest_dir).join("swift/avf-runner/.build/release/agv-avf-runner");
    if path.exists() { Some(path) } else { None }
}

/// Symlink the Swift runner next to the `agv` binary so the
/// production resolver (`locate_avf_runner`, in `src/vm/backend.rs`)
/// picks it up via its sibling-of-current-exe fallback.
///
/// Symlink, not copy: macOS's AppleSystemPolicy provenance sandbox
/// invalidates the code signature when a Mach-O is copied to a new
/// path. The kernel logs:
///
///     proc <pid>: load code signature error 2 for file "agv-avf-runner"
///     ASP: Unable to apply provenance sandbox: ...
///
/// and the runner gets SIGKILL'd before it can write anything to
/// stdout/stderr. A symlink points at the original signed binary,
/// preserving provenance. Tests assume the developer has run
/// `just build-avf-runner`; production installs ship a real binary
/// in the same dir as agv via the release tarball, which goes
/// through proper distribution and avoids the sandbox issue.
fn ensure_runner_alongside_agv(source: &Path) {
    let agv_bin = PathBuf::from(env!("CARGO_BIN_EXE_agv"));
    let dest = agv_bin
        .parent()
        .expect("agv binary has a parent dir")
        .join("agv-avf-runner");
    // Idempotent: if a symlink to the right target already exists, no-op.
    if let Ok(cur) = std::fs::read_link(&dest) {
        if cur == source {
            return;
        }
    }
    let _ = std::fs::remove_file(&dest);
    std::os::unix::fs::symlink(source, &dest)
        .expect("symlink runner alongside agv binary");
}

/// Pre-configured `agv` subprocess command pointed at the test's
/// isolated `AGV_DATA_DIR`. `--quiet` keeps spinner output off
/// stdout so `--json` parses cleanly.
fn agv(data_dir: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_agv"));
    cmd.env("AGV_DATA_DIR", data_dir).arg("--quiet");
    cmd
}

/// Write a config TOML body to `<host_dir>/agv.toml` and return its path.
async fn write_config(host_dir: &Path, body: &str) -> PathBuf {
    let path = host_dir.join("agv.toml");
    tokio::fs::write(&path, body).await.unwrap();
    path
}

/// Parse stdout as JSON, panicking with a useful diagnostic on a
/// shape mismatch.
fn parse_json(label: &str, stdout: &[u8]) -> serde_json::Value {
    let s = String::from_utf8(stdout.to_vec()).unwrap();
    serde_json::from_str(s.trim()).unwrap_or_else(|e| {
        panic!("{label} stdout didn't parse as JSON: {e}\nstdout:\n{s}")
    })
}

/// Run `agv inspect <name> --json` and return the parsed report.
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

/// Best-effort destroy. Always called from a cleanup branch so a
/// failure mid-test doesn't leave an AVF runner alive holding the
/// disposable tempdir.
async fn destroy(data_dir: &Path, name: &str) {
    let _ = agv(data_dir)
        .args(["destroy", "--force", name])
        .output()
        .await;
}

/// Full AVF lifecycle end-to-end through `agv create --backend avf`:
/// downloads Debian 12, converts qcow2→raw, boots under AVF,
/// SSHes in, suspends, resumes, asserts SSH still works on the
/// restored VM, then destroys.
///
/// Reuses `AGV_DATA_DIR/cache/images/` if the Debian raw is already
/// cached from a prior run, so subsequent runs skip the download.
#[tokio::test]
#[ignore = "boots a real AVF VM end-to-end through the agv CLI — slow, ~60s"]
#[serial]
async fn agv_create_start_suspend_resume_destroy() {
    let Some(runner) = runner_binary() else {
        eprintln!(
            "agv-avf-runner not built — skipping agv_create_start_suspend_resume_destroy (run: just build-avf-runner)"
        );
        return;
    };
    ensure_runner_alongside_agv(&runner);

    let data_dir = tempfile::tempdir().unwrap();
    let host_tmp = tempfile::tempdir().unwrap();
    // Per-run unique name. Two constraints:
    //   1. RFC-1123 hostname — no leading underscore. AVF's lease
    //      lookup keys on the guest's hostname, and systemd-networkd
    //      sanitises a name starting with `_` before writing the
    //      DHCP request. (`_test-...` works for QEMU tests because
    //      their SSH path is a port forward, not DHCP discovery.)
    //   2. Unique-per-run to avoid stale leases — `/var/db/dhcpd_leases`
    //      persists across test runs and a stale entry points the
    //      runner at the previous VM's IP, causing SSH to time out
    //      against a host that isn't there anymore.
    // Suffix kept short (6 hex chars) so the resulting unix-socket
    // path stays under macOS's 104-byte sun_path limit. Tempdir
    // prefixes alone eat ~60 bytes of that budget.
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let suffix = format!("{:06x}", (ts as u64) & 0xff_ffff);
    let name = format!("avf-e2e-{suffix}");
    let name = name.as_str();

    let config_toml = r#"
[base]
from = "debian-12"

[vm]
memory = "1G"
cpus = 2
disk = "3G"
backend = "avf"
"#;
    let toml_path = write_config(host_tmp.path(), config_toml).await;

    // --- create + start ---
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
        // Capture artifacts before tempdir cleanup so the failure
        // mode is debuggable. Serial logs and runner logs are the
        // only signal we get from a VM that wouldn't come up.
        let inst_dir = data_dir.path().join("instances").join(name);
        let serial = std::fs::read_to_string(inst_dir.join("serial.log"))
            .unwrap_or_default();
        let runner_log = std::fs::read_to_string(inst_dir.join("avf-runner.log"))
            .unwrap_or_default();
        destroy(data_dir.path(), name).await;
        panic!(
            "agv create failed (exit {:?}): {}\nstdout:\n{}\n--- serial.log (tail 2000) ---\n{}\n--- avf-runner.log ---\n{}",
            create_output.status.code(),
            String::from_utf8_lossy(&create_output.stderr),
            String::from_utf8_lossy(&create_output.stdout),
            serial.chars().rev().take(2000).collect::<String>().chars().rev().collect::<String>(),
            runner_log,
        );
    }
    let report = parse_json("agv create", &create_output.stdout);
    if report["status"] != "running" {
        let inst_dir = data_dir.path().join("instances").join(name);
        let serial = std::fs::read_to_string(inst_dir.join("serial.log"))
            .unwrap_or_default();
        let runner_log = std::fs::read_to_string(inst_dir.join("avf-runner.log"))
            .unwrap_or_default();
        let provision_log = std::fs::read_to_string(inst_dir.join("provision.log"))
            .unwrap_or_default();
        let error_log = std::fs::read_to_string(inst_dir.join("error.log"))
            .unwrap_or_default();
        destroy(data_dir.path(), name).await;
        panic!(
            "VM status is {:?}, expected 'running'. Full report:\n{report:#?}\n\
             --- runner log ---\n{runner_log}\n\
             --- provision.log ---\n{provision_log}\n\
             --- error.log ---\n{error_log}\n\
             --- serial.log (last 2000 chars) ---\n{}",
            report["status"],
            serial.chars().rev().take(2000).collect::<String>().chars().rev().collect::<String>(),
        );
    }
    assert_eq!(report["backend"], "avf", "VM should record backend=avf");

    let inst_dir = PathBuf::from(report["data_dir"].as_str().unwrap());
    // AVF-specific artifacts should land under the instance dir.
    for f in ["disk.raw", "avf-runner.pid", "avf-control.sock", "avf-mac", "avf-machine-id"] {
        assert!(
            inst_dir.join(f).exists(),
            "AVF artifact {f} should exist at {}",
            inst_dir.join(f).display(),
        );
    }
    // QEMU artifacts should NOT exist for an AVF VM.
    assert!(
        !inst_dir.join("disk.qcow2").exists(),
        "disk.qcow2 must not exist for an AVF VM"
    );
    assert!(
        !inst_dir.join("pid").exists(),
        "QEMU pid file must not exist for an AVF VM"
    );

    // --- SSH works ---
    let ssh_output = agv(data_dir.path())
        .args(["ssh", name, "--", "whoami"])
        .output()
        .await
        .unwrap();
    if !ssh_output.status.success() {
        destroy(data_dir.path(), name).await;
        panic!(
            "agv ssh {name} -- whoami failed: {}",
            String::from_utf8_lossy(&ssh_output.stderr),
        );
    }
    let who = String::from_utf8_lossy(&ssh_output.stdout);
    assert!(
        who.trim() == "agent",
        "SSH should run as 'agent', got: {who:?}"
    );

    // --- suspend ---
    let suspend_output = agv(data_dir.path())
        .args(["suspend", name])
        .output()
        .await
        .unwrap();
    if !suspend_output.status.success() {
        destroy(data_dir.path(), name).await;
        panic!(
            "agv suspend failed: {}",
            String::from_utf8_lossy(&suspend_output.stderr),
        );
    }
    let suspended = inspect(data_dir.path(), name).await;
    assert_eq!(
        suspended["status"], "suspended",
        "VM status should be 'suspended' after agv suspend"
    );
    assert!(
        inst_dir.join("avf-snapshot.bin").exists(),
        "snapshot file should land at <inst>/avf-snapshot.bin",
    );

    // --- resume ---
    let resume_output = agv(data_dir.path())
        .args(["resume", name])
        .output()
        .await
        .unwrap();
    if !resume_output.status.success() {
        let runner_log = std::fs::read_to_string(inst_dir.join("avf-runner.log"))
            .unwrap_or_default();
        destroy(data_dir.path(), name).await;
        panic!(
            "agv resume failed: {}\n--- runner log ---\n{runner_log}\nstdout: {}",
            String::from_utf8_lossy(&resume_output.stderr),
            String::from_utf8_lossy(&resume_output.stdout),
        );
    }
    let resumed = inspect(data_dir.path(), name).await;
    assert_eq!(
        resumed["status"], "running",
        "VM status should be 'running' after agv resume"
    );
    assert!(
        !inst_dir.join("avf-snapshot.bin").exists(),
        "snapshot file should be cleaned up after successful resume",
    );

    // SSH must still work on the resumed VM — the whole point of
    // resume is that the guest comes back from where it left off.
    let ssh_after_resume = agv(data_dir.path())
        .args(["ssh", name, "--", "whoami"])
        .output()
        .await
        .unwrap();
    if !ssh_after_resume.status.success() {
        destroy(data_dir.path(), name).await;
        panic!(
            "agv ssh after resume failed: {}",
            String::from_utf8_lossy(&ssh_after_resume.stderr),
        );
    }
    let who2 = String::from_utf8_lossy(&ssh_after_resume.stdout);
    assert!(
        who2.trim() == "agent",
        "SSH after resume should run as 'agent', got: {who2:?}"
    );

    destroy(data_dir.path(), name).await;
}
