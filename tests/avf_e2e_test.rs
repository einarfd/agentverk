//! End-to-end integration test for the AVF backend through the
//! `agv` CLI — the same path real users hit. Boots a Debian VM under
//! Apple Virtualization, asserts SSH works, locks in the AVF
//! suspend-refusal contract (Apple's framework doesn't support
//! save/restore for Linux guests — see `src/vm/mod.rs::suspend`),
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
// Test-pragmatism lints. macOS-only file; pinned at file scope per
// the AGENTS.md convention (use `#[expect]` over `#[allow]`).
#![expect(
    clippy::doc_markdown,
    reason = "test docstrings quote tool / path names freely without backticks"
)]
#![expect(
    clippy::map_unwrap_or,
    reason = "test code prefers the verb form for readability"
)]
#![expect(
    clippy::cast_possible_truncation,
    reason = "nanos-since-epoch ANDed with 0xff_ffff is a deliberate 24-bit hash for unique test VM names"
)]

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
/// SSHes in, asserts the AVF suspend refusal is surfaced cleanly
/// (and that the VM stays usable after the refusal — proving the
/// early-bail in `vm::suspend` doesn't tear down forwards/watcher),
/// then destroys.
///
/// Reuses `AGV_DATA_DIR/cache/images/` if the Debian raw is already
/// cached from a prior run, so subsequent runs skip the download.
#[tokio::test]
#[ignore = "boots a real AVF VM end-to-end through the agv CLI — slow, ~60s"]
#[serial]
async fn agv_create_start_ssh_suspend_refused_destroy() {
    let Some(runner) = runner_binary() else {
        eprintln!(
            "agv-avf-runner not built — skipping agv_create_start_ssh_suspend_refused_destroy (run: just build-avf-runner)"
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

    // --- managed ssh_config entry must use guest IP, not localhost ---
    // Regression: `update_ssh_config` previously read `ssh_port_path()`
    // (QEMU-only) and silently skipped AVF VMs, so a fresh `ssh <name>`
    // from the host's shell would either fail with "no such host" or
    // try to connect to whatever was on the host's own port 22.
    let managed = std::fs::read_to_string(data_dir.path().join("ssh_config"))
        .expect("managed ssh_config should exist after a successful start");
    assert!(
        managed.contains(&format!("Host {name}")),
        "managed ssh_config should have a Host block for {name}; got:\n{managed}"
    );
    assert!(
        !managed.contains("HostName localhost"),
        "AVF entry must not write `HostName localhost` (that's the QEMU-port-forward shape); got:\n{managed}"
    );
    // AVF VMs always SSH to port 22 on the guest — the QEMU `-hostfwd`
    // port allocation doesn't apply.
    assert!(
        managed.contains("Port 22"),
        "managed ssh_config for an AVF VM should target the guest's port 22; got:\n{managed}"
    );

    // --- suspend is refused for AVF (Apple framework limitation) ---
    // Apple Virtualization framework does not support save/restore
    // for Linux guests — `agv suspend` refuses early (before any
    // teardown) with a clear, actionable error. This test locks in
    // both the refusal AND that the VM stays in a usable state
    // afterwards (status=running, SSH still works, no snapshot file
    // written).
    let suspend_output = agv(data_dir.path())
        .args(["suspend", name])
        .output()
        .await
        .unwrap();
    assert!(
        !suspend_output.status.success(),
        "agv suspend on an AVF VM must fail — Linux save/restore is unsupported by the framework"
    );
    let stderr = String::from_utf8_lossy(&suspend_output.stderr);
    assert!(
        stderr.contains("avf backend") && stderr.contains("Apple Virtualization framework"),
        "refusal must mention the framework limitation; got stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("agv stop") || stderr.contains("--backend qemu"),
        "refusal must point users at the workaround; got stderr:\n{stderr}"
    );
    // Critical: refusal must not write a misleading snapshot file
    // on disk. If the file existed, a follow-up `agv resume` would
    // attempt a restore that will Code=12 mid-restore — a much
    // worse failure mode than the early refusal.
    assert!(
        !inst_dir.join("avf-snapshot.bin").exists(),
        "refused suspend must not have created a snapshot at <inst>/avf-snapshot.bin"
    );
    // Status stays running — the early refusal in `vm::suspend`
    // bails before touching the idle watcher or port forwards.
    let after_refuse = inspect(data_dir.path(), name).await;
    assert_eq!(
        after_refuse["status"], "running",
        "VM should still be running after a refused suspend"
    );
    // SSH still works — proves the early refusal didn't tear down
    // the live VM's networking / process state.
    let ssh_after_refuse = agv(data_dir.path())
        .args(["ssh", name, "--", "whoami"])
        .output()
        .await
        .unwrap();
    if !ssh_after_refuse.status.success() {
        destroy(data_dir.path(), name).await;
        panic!(
            "agv ssh after refused suspend failed — the early-refuse path \
             must not disturb the running VM. Stderr: {}",
            String::from_utf8_lossy(&ssh_after_refuse.stderr),
        );
    }
    let who2 = String::from_utf8_lossy(&ssh_after_refuse.stdout);
    assert!(
        who2.trim() == "agent",
        "SSH after refused suspend should still run as 'agent', got: {who2:?}"
    );

    destroy(data_dir.path(), name).await;
}
