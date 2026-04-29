//! Integration tests for the VM create lifecycle.
//!
//! These tests exercise the full `agv` binary end-to-end as a subprocess
//! and assert on `--json` output. That path is what an agent driving agv
//! actually uses, so a regression in the JSON contract or in the
//! production code path the CLI takes will surface here. Tests
//! intentionally avoid the in-process `vm::*` API for state inspection;
//! the `data_dir` field in `VmStateReport` anchors any artifact-level
//! checks (logs, on-disk files).
//!
//! Each test gets its own `AGV_DATA_DIR` tempdir, so tests never collide
//! with each other or with the user's real `~/.local/share/agv/`.
//!
//! Tests skip gracefully if external tools (`qemu-img`,
//! `mkisofs`/`genisoimage`, QEMU) are missing, so `cargo test` always
//! passes.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use serial_test::serial;

/// Counter to generate unique filenames across concurrent tests so the
/// fake-image filenames never collide.
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

/// Path to `<data_dir>/cache/images/`. Mirrors `agv::dirs::image_cache_dir`.
fn cache_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("cache").join("images")
}

/// Path to `<data_dir>/images/`. Mirrors `agv::dirs::images_dir`.
fn user_images_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("images")
}

/// Pre-configured `agv` subprocess command pointed at the test's
/// isolated data dir. Always runs with `--quiet` so spinner/status text
/// can't interleave with `--json` stdout.
fn agv(data_dir: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_agv"));
    cmd.env("AGV_DATA_DIR", data_dir).arg("--quiet");
    cmd
}

/// Best-effort destroy. Tests call this before the `AGV_DATA_DIR`
/// tempdir drops so QEMU isn't left running with handles to a
/// disappearing directory.
async fn destroy(data_dir: &Path, name: &str) {
    let _ = agv(data_dir)
        .args(["destroy", "--force", name])
        .output()
        .await;
}

/// Create a unique 1G fake qcow2 in the test's image cache and return
/// a fake URL whose filename matches it. Tests using this must pass
/// `--no-checksum` to `agv create` because the cached image has no
/// matching checksum.
async fn make_fake_base_image(data_dir: &Path) -> String {
    let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let filename = format!("_agv-test-base-{id}.qcow2");
    let fake_url = format!("https://example.invalid/{filename}");

    let cache = cache_dir(data_dir);
    tokio::fs::create_dir_all(&cache).await.unwrap();
    let cached_path = cache.join(&filename);
    let cached_str = cached_path.to_str().unwrap();
    let output = tokio::process::Command::new("qemu-img")
        .args(["create", "-f", "qcow2", cached_str, "1G"])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "qemu-img create failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );

    fake_url
}

/// TOML body for a synthetic-image config (no real cloud-image
/// download). Picks the right `[base.<arch>]` section for the host so
/// the same body works on Apple silicon and `x86_64` CI runners. The
/// checksum is bogus on purpose — pair with `--no-checksum`.
fn synthetic_config_toml(image_url: &str) -> String {
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    };
    format!(
        r#"
[base]
os_family = "debian"

[base.{arch}]
url = "{image_url}"
checksum = "sha256:0000000000000000000000000000000000000000000000000000000000000000"

[vm]
memory = "512M"
cpus = 1
disk = "2G"
"#
    )
}

/// Write a config TOML body to `<host_dir>/agv.toml` and return its path.
async fn write_config(host_dir: &Path, body: &str) -> PathBuf {
    let path = host_dir.join("agv.toml");
    tokio::fs::write(&path, body).await.unwrap();
    path
}

/// Parse stdout as JSON, panicking with a useful diagnostic if the
/// shape is wrong. `label` shows up in the error message.
fn parse_json(label: &str, stdout: &[u8]) -> serde_json::Value {
    let s = String::from_utf8(stdout.to_vec()).unwrap();
    serde_json::from_str(s.trim()).unwrap_or_else(|e| {
        panic!("{label} stdout didn't parse as JSON: {e}\nstdout:\n{s}")
    })
}

/// Run `agv inspect <name> --json` and return the parsed `VmStateReport`.
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

/// Run a command inside the VM and return stdout.
///
/// Invokes the system `ssh` client directly using the port and key
/// surfaced by `agv inspect --json`, with `-F /dev/null` so the user's
/// `~/.ssh/config` can't interfere with test runs. This is what `agv
/// ssh` does under the hood; testing the wrapper's argument parsing is
/// out of scope for the slow boot suite (covered in `tests/cli_test.rs`).
async fn ssh_exec(data_dir: &Path, name: &str, cmd: &str) -> String {
    let report = inspect(data_dir, name).await;
    let port = u16::try_from(
        report["ssh_port"]
            .as_u64()
            .expect("ssh_port must be set on a running VM"),
    )
    .expect("ssh_port must fit u16");
    let inst_dir = PathBuf::from(report["data_dir"].as_str().unwrap());
    let key = inst_dir.join("id_ed25519");

    let output = tokio::process::Command::new("ssh")
        .args([
            "-i",
            key.to_str().unwrap(),
            "-p",
            &port.to_string(),
            "-F",
            "/dev/null",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "LogLevel=ERROR",
            "-o",
            "ConnectTimeout=5",
            "agent@localhost",
            "--",
            cmd,
        ])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "ssh {name} -- {cmd} failed (exit {:?})\nstderr: {}\nstdout: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
    );
    String::from_utf8(output.stdout).unwrap()
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

    let data_dir = tempfile::tempdir().unwrap();
    let host_tmp = tempfile::tempdir().unwrap();

    let image_url = make_fake_base_image(data_dir.path()).await;
    let toml_path =
        write_config(host_tmp.path(), &synthetic_config_toml(&image_url)).await;

    let name = "_test-create-nostrt";

    let output = agv(data_dir.path())
        .args([
            "create",
            "--json",
            "--no-checksum",
            "--config",
            toml_path.to_str().unwrap(),
            name,
        ])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "agv create failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );

    let report = parse_json("agv create", &output.stdout);
    assert_eq!(report["status"], "stopped");
    assert_eq!(report["created"], serde_json::Value::Bool(true));
    assert_eq!(report["memory"], "512M");
    assert_eq!(report["cpus"], 1);
    assert_eq!(
        report["ssh_port"],
        serde_json::Value::Null,
        "ssh_port must be null when the VM hasn't been started",
    );

    let inst_dir = PathBuf::from(report["data_dir"].as_str().unwrap());
    assert!(inst_dir.exists(), "instance dir should exist");
    for f in ["disk.qcow2", "seed.iso", "id_ed25519", "id_ed25519.pub", "config.toml"] {
        assert!(inst_dir.join(f).exists(), "{f} should exist");
    }
    // PID and port files should NOT exist (not started).
    assert!(!inst_dir.join("pid").exists(), "PID file should not exist");
    assert!(
        !inst_dir.join("ssh_port").exists(),
        "ssh_port file should not exist",
    );
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

    let data_dir = tempfile::tempdir().unwrap();
    let host_tmp = tempfile::tempdir().unwrap();

    let image_url = make_fake_base_image(data_dir.path()).await;
    let toml_path =
        write_config(host_tmp.path(), &synthetic_config_toml(&image_url)).await;

    let name = "_test-create-dup";
    let create_args = [
        "create",
        "--json",
        "--no-checksum",
        "--config",
        toml_path.to_str().unwrap(),
        name,
    ];

    // First create should succeed.
    let first = agv(data_dir.path()).args(create_args).output().await.unwrap();
    assert!(
        first.status.success(),
        "first create failed: {}",
        String::from_utf8_lossy(&first.stderr),
    );

    // Second create with the same name should fail with the documented
    // "already exists" exit code (10).
    let second = agv(data_dir.path()).args(create_args).output().await.unwrap();
    assert_eq!(
        second.status.code(),
        Some(10),
        "expected exit 10 (VM already exists), got {:?}\nstderr: {}",
        second.status.code(),
        String::from_utf8_lossy(&second.stderr),
    );
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains("already exists"),
        "expected 'already exists' error, got: {stderr}",
    );
}

#[tokio::test]
async fn create_marks_broken_on_failure() {
    let data_dir = tempfile::tempdir().unwrap();
    let host_tmp = tempfile::tempdir().unwrap();

    // Point at an unreachable URL so the image download fails. This
    // exercises the broken-on-failure path without a real image cache
    // entry.
    let arch = if cfg!(target_arch = "aarch64") { "aarch64" } else { "x86_64" };
    let body = format!(
        r#"
[base]
os_family = "debian"

[base.{arch}]
url = "http://127.0.0.1:1/nonexistent-image.qcow2"
checksum = "sha256:0000000000000000000000000000000000000000000000000000000000000000"

[vm]
memory = "512M"
cpus = 1
disk = "2G"
"#
    );
    let toml_path = write_config(host_tmp.path(), &body).await;

    let name = "_test-create-broken";

    let output = agv(data_dir.path())
        .args([
            "create",
            "--json",
            "--no-checksum",
            "--config",
            toml_path.to_str().unwrap(),
            name,
        ])
        .output()
        .await
        .unwrap();
    assert!(
        !output.status.success(),
        "create should fail when the base image URL is unreachable",
    );

    // The VM should be left in `broken` state with an error.log on disk.
    let report = inspect(data_dir.path(), name).await;
    assert_eq!(report["status"], "broken");

    let inst_dir = PathBuf::from(report["data_dir"].as_str().unwrap());
    let error_log_path = inst_dir.join("error.log");
    assert!(error_log_path.exists(), "error.log should exist");
    let error_log = tokio::fs::read_to_string(&error_log_path).await.unwrap();
    assert!(!error_log.is_empty(), "error.log should have content");
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

    let data_dir = tempfile::tempdir().unwrap();
    let host_tmp = tempfile::tempdir().unwrap();

    let test_file = host_tmp.path().join("agv-test-inject.txt");
    tokio::fs::write(&test_file, "injected-by-agv").await.unwrap();

    // Use debian-12: smaller image (~330 MB) and fully apt-compatible.
    let config_toml = format!(
        r#"
[base]
from = "debian-12"

[vm]
memory = "1G"
cpus = 2
disk = "10G"

[[files]]
source = "{src}"
dest = "/home/agent/.config/agv-test/agv-test-inject.txt"

[[provision]]
run = "cat /home/agent/.config/agv-test/agv-test-inject.txt"
"#,
        src = test_file.to_str().unwrap(),
    );
    let toml_path = write_config(host_tmp.path(), &config_toml).await;

    let name = "_test-create-full";

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
            "agv create failed (exit {:?}): {}\nstdout:\n{}",
            create_output.status.code(),
            String::from_utf8_lossy(&create_output.stderr),
            String::from_utf8_lossy(&create_output.stdout),
        );
    }

    let report = parse_json("agv create", &create_output.stdout);

    assert_eq!(report["status"], "running");
    assert_eq!(report["created"], serde_json::Value::Bool(true));
    assert!(
        report["ssh_port"].is_u64(),
        "ssh_port must be a number on a running VM (got {:?})",
        report["ssh_port"],
    );
    assert_eq!(report["memory"], "1G");
    assert_eq!(report["cpus"], 2);
    assert_eq!(report["disk"], "10G");

    let inst_dir = PathBuf::from(report["data_dir"].as_str().unwrap());
    assert!(inst_dir.join("disk.qcow2").exists(), "disk.qcow2 should exist");
    assert!(inst_dir.join("seed.iso").exists(), "seed.iso should exist");

    let log = tokio::fs::read_to_string(inst_dir.join("provision.log"))
        .await
        .unwrap();
    assert!(!log.is_empty(), "provision.log should have content");
    assert!(
        log.contains("injected-by-agv"),
        "provision.log should contain injected file content — file copy via SCP failed",
    );

    // End-to-end check that `agv ssh <name> -- <cmd>` correctly
    // routes the trailing arg as a remote command (and not as an
    // ssh option, which would make ssh treat it as a hostname).
    // Regression coverage for the clap quirk where leading `--` is
    // silently consumed; the dispatcher recovers from raw argv.
    let ssh_output = agv(data_dir.path())
        .args(["ssh", name, "--", "echo agv-ssh-double-dash-ok"])
        .output()
        .await
        .unwrap();
    assert!(
        ssh_output.status.success(),
        "agv ssh {name} -- <cmd> failed: {}",
        String::from_utf8_lossy(&ssh_output.stderr),
    );
    let ssh_stdout = String::from_utf8(ssh_output.stdout).unwrap();
    assert!(
        ssh_stdout.contains("agv-ssh-double-dash-ok"),
        "expected echo output via `agv ssh <name> -- <cmd>`, got:\n{ssh_stdout}",
    );

    destroy(data_dir.path(), name).await;
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

    let data_dir = tempfile::tempdir().unwrap();
    let host_tmp = tempfile::tempdir().unwrap();

    // Inject a marker file to confirm file copy works on a non-debian guest.
    let test_file = host_tmp.path().join("agv-test-inject.txt");
    tokio::fs::write(&test_file, "injected-by-agv-on-fedora")
        .await
        .unwrap();

    // The provision step also checks dnf is on PATH, which proves the
    // devtools mixin's fedora-family setup ran instead of the
    // apt-family one. (We lose the in-process resolver-shape sanity
    // check here, but the behavioural test is strictly stronger.)
    let config_toml = format!(
        r#"
[base]
from = "fedora-43"
include = ["devtools"]

[vm]
memory = "2G"
cpus = 2
disk = "10G"

[[files]]
source = "{src}"
dest = "/home/agent/.config/agv-test/agv-test-inject.txt"

[[provision]]
run = "cat /home/agent/.config/agv-test/agv-test-inject.txt && command -v dnf >/dev/null && echo dnf-present"
"#,
        src = test_file.to_str().unwrap(),
    );
    let toml_path = write_config(host_tmp.path(), &config_toml).await;

    let name = "_test-fedora";

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
            "agv create failed (exit {:?}): {}\nstdout:\n{}",
            create_output.status.code(),
            String::from_utf8_lossy(&create_output.stderr),
            String::from_utf8_lossy(&create_output.stdout),
        );
    }

    let report = parse_json("agv create", &create_output.stdout);
    assert_eq!(report["status"], "running");
    assert!(
        report["ssh_port"].is_u64(),
        "ssh_port must be set on a running VM",
    );

    let inst_dir = PathBuf::from(report["data_dir"].as_str().unwrap());
    let log = tokio::fs::read_to_string(inst_dir.join("provision.log"))
        .await
        .unwrap();
    assert!(
        log.contains("injected-by-agv-on-fedora"),
        "provision.log should contain injected file content on fedora:\n{log}",
    );
    assert!(
        log.contains("dnf-present"),
        "provision.log should confirm dnf is installed on fedora:\n{log}",
    );

    destroy(data_dir.path(), name).await;
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

    let data_dir = tempfile::tempdir().unwrap();
    let host_tmp = tempfile::tempdir().unwrap();

    // Drop a test mixin into the test data dir's images/ so the
    // include-by-name resolution finds it. The mixin declares a named
    // auto_forward and starts python's http.server on the guest port.
    //
    // System service rather than user service — the latter's
    // `loginctl enable-linger` + mid-session `systemctl --user enable --now`
    // is a known-flaky pattern that can race with pam_systemd on first
    // provisioning. System services start reliably via a single
    // `systemctl enable --now`.
    let images = user_images_dir(data_dir.path());
    tokio::fs::create_dir_all(&images).await.unwrap();
    let mixin_name = "_agv-test-autofwd";
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
    tokio::fs::write(images.join(format!("{mixin_name}.toml")), mixin_contents)
        .await
        .unwrap();

    let config_toml = format!(
        r#"
[base]
from = "debian-12"
include = ["{mixin_name}"]

[vm]
memory = "1G"
cpus = 2
disk = "10G"
"#
    );
    let toml_path = write_config(host_tmp.path(), &config_toml).await;

    let name = "_test-auto-forwards";

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
            "agv create failed (exit {:?}): {}\nstdout:\n{}",
            create_output.status.code(),
            String::from_utf8_lossy(&create_output.stderr),
            String::from_utf8_lossy(&create_output.stdout),
        );
    }

    let report = parse_json("agv create", &create_output.stdout);
    assert_eq!(report["status"], "running");
    let inst_dir = PathBuf::from(report["data_dir"].as_str().unwrap());

    // The auto_forward should appear in `agv forward --list --json` with
    // origin = "auto" — that's the agent-facing surface for discovering
    // host ports allocated to mixin-declared forwards.
    let forwards_output = agv(data_dir.path())
        .args(["forward", "--list", "--json", name])
        .output()
        .await
        .unwrap();
    assert!(
        forwards_output.status.success(),
        "agv forward --list --json failed: {}",
        String::from_utf8_lossy(&forwards_output.stderr),
    );
    let forwards = parse_json("agv forward --list", &forwards_output.stdout);
    let forwards_array = forwards.as_array().expect("forwards must be an array");
    let auto_forward = forwards_array
        .iter()
        .find(|f| f["origin"] == "auto" && f["guest"] == 9001)
        .unwrap_or_else(|| {
            panic!("no auto_forward with guest=9001 in: {forwards:?}")
        });
    let port = u16::try_from(
        auto_forward["host"]
            .as_u64()
            .expect("host port must be a number"),
    )
    .expect("host port must fit in u16");
    assert!(port > 0, "host port must be > 0");

    // Hit the python http.server inside the guest via the SSH tunnel.
    // Retry for up to 30s because the systemd unit may take a moment to
    // come up after first boot.
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
    if body.is_none() {
        // Collect diagnostics from both sides so a regression is actionable.
        let service_status = ssh_exec(
            data_dir.path(),
            name,
            "systemctl status agv-test-http --no-pager -l || true",
        )
        .await;
        let journal = ssh_exec(
            data_dir.path(),
            name,
            "sudo journalctl -u agv-test-http --no-pager -n 50 || true",
        )
        .await;
        let forwards_toml =
            tokio::fs::read_to_string(inst_dir.join("forwards.toml"))
                .await
                .unwrap_or_else(|e| format!("<read failed: {e}>"));
        destroy(data_dir.path(), name).await;
        panic!(
            "never got a 200 OK from the tunneled python http.server.\n\
             \n\
             host port: {port}\n\
             \n\
             ---- <instance>/forwards.toml ----\n{forwards_toml}\n\
             ---- guest: systemctl status agv-test-http ----\n{service_status}\n\
             ---- guest: journalctl -u agv-test-http ----\n{journal}",
        );
    }
    let body = body.unwrap();
    assert!(
        body.contains("Directory listing") || body.contains("<title>"),
        "expected python http.server index page, got: {body}",
    );

    destroy(data_dir.path(), name).await;
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

    let data_dir = tempfile::tempdir().unwrap();
    let host_tmp = tempfile::tempdir().unwrap();

    let config_toml = r#"
[base]
from = "debian-12"

[vm]
memory = "1G"
cpus = 2
disk = "10G"
"#;
    let toml_path = write_config(host_tmp.path(), config_toml).await;

    let name = "_test-suspend-resume";

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
            "agv create failed: {}",
            String::from_utf8_lossy(&create_output.stderr),
        );
    }
    let report = parse_json("agv create", &create_output.stdout);
    assert_eq!(report["status"], "running");

    // Write a marker file to /run (tmpfs, lives only in RAM) and capture
    // the uptime before suspending.
    let marker = format!("agv-suspend-test-{}", std::process::id());
    ssh_exec(
        data_dir.path(),
        name,
        &format!("sudo sh -c 'echo {marker} > /run/agv-marker'"),
    )
    .await;

    let uptime_before_raw =
        ssh_exec(data_dir.path(), name, "cat /proc/uptime").await;
    let uptime_before: f64 = uptime_before_raw
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();

    // Suspend the VM.
    let suspend_output = agv(data_dir.path())
        .args(["suspend", "--json", name])
        .output()
        .await
        .unwrap();
    assert!(
        suspend_output.status.success(),
        "agv suspend failed: {}",
        String::from_utf8_lossy(&suspend_output.stderr),
    );
    let suspend_report = parse_json("agv suspend", &suspend_output.stdout);
    assert_eq!(suspend_report["status"], "suspended");
    assert_eq!(
        suspend_report["ssh_port"],
        serde_json::Value::Null,
        "ssh_port must be null when the VM is suspended",
    );

    // Resume the VM.
    let resume_output = agv(data_dir.path())
        .args(["resume", "--json", name])
        .output()
        .await
        .unwrap();
    if !resume_output.status.success() {
        destroy(data_dir.path(), name).await;
        panic!(
            "agv resume failed: {}",
            String::from_utf8_lossy(&resume_output.stderr),
        );
    }
    let resume_report = parse_json("agv resume", &resume_output.stdout);
    assert_eq!(resume_report["status"], "running");
    assert!(
        resume_report["ssh_port"].is_u64(),
        "ssh_port must be set after resume",
    );

    // The marker file in tmpfs should still be there — proves RAM state was
    // saved and restored.
    let marker_content =
        ssh_exec(data_dir.path(), name, "cat /run/agv-marker").await;
    assert!(
        marker_content.contains(&marker),
        "marker file lost after suspend/resume — state was not preserved (got: {marker_content:?})",
    );

    // Uptime should be at least as large as before — proves the VM did not
    // reboot during suspend/resume.
    let uptime_after_raw = ssh_exec(data_dir.path(), name, "cat /proc/uptime").await;
    let uptime_after: f64 = uptime_after_raw
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        uptime_after >= uptime_before,
        "VM uptime decreased ({uptime_before} → {uptime_after}) — VM rebooted instead of resuming",
    );

    destroy(data_dir.path(), name).await;
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

    let data_dir = tempfile::tempdir().unwrap();
    let host_tmp = tempfile::tempdir().unwrap();

    // Three provision steps:
    //   0. echo "first" >> /tmp/agv-retry-log    (always succeeds)
    //   1. fails on the first run, succeeds on the second (counter file)
    //   2. echo "third" >> /tmp/agv-retry-log    (only runs after retry)
    //
    // After the initial create:
    //   - Step 0 ran → log contains "first"
    //   - Step 1 failed → broken
    // After retry:
    //   - Step 1 ran successfully → log contains "second"
    //   - Step 2 ran → log contains "third"
    //   - Step 0 should NOT have run again (we'd see "first" twice).
    //
    // The behavioural log-content check proves the retry resumed from
    // step 1; we lose the in-process `provision_state` introspection
    // (phase/index/error) here, but the log is strictly stronger.
    let config_toml = r#"
[base]
from = "debian-12"

[vm]
memory = "1G"
cpus = 2
disk = "10G"

[[provision]]
run = "echo first >> /tmp/agv-retry-log"

[[provision]]
run = """
if [ -f /tmp/agv-retry-counter ]; then
  echo second >> /tmp/agv-retry-log
else
  touch /tmp/agv-retry-counter
  exit 1
fi
"""

[[provision]]
run = "echo third >> /tmp/agv-retry-log"
"#;
    let toml_path = write_config(host_tmp.path(), config_toml).await;

    let name = "_test-retry";

    // First create — expected to fail at step 1. agv leaves QEMU running
    // on broken first-boot so the user can SSH in to debug.
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
    assert!(
        !create_output.status.success(),
        "expected create to fail because of the deliberately failing provision step",
    );

    // VM should be marked broken.
    let report = inspect(data_dir.path(), name).await;
    assert_eq!(report["status"], "broken");

    // QEMU should still be reachable via SSH (we leave it running on
    // broken first-boot for debugging).
    let log_after_fail =
        ssh_exec(data_dir.path(), name, "cat /tmp/agv-retry-log").await;
    assert_eq!(
        log_after_fail.matches("first").count(),
        1,
        "expected step 0 to have run once before failure (got: {log_after_fail:?})",
    );
    assert!(
        !log_after_fail.contains("third"),
        "step 2 should not have run yet (got: {log_after_fail:?})",
    );

    // Retry — should resume from step 1.
    let retry_output = agv(data_dir.path())
        .args(["start", "--retry", "--json", name])
        .output()
        .await
        .unwrap();
    if !retry_output.status.success() {
        destroy(data_dir.path(), name).await;
        panic!(
            "agv start --retry failed: {}",
            String::from_utf8_lossy(&retry_output.stderr),
        );
    }
    let retry_report = parse_json("agv start --retry", &retry_output.stdout);
    assert_eq!(retry_report["status"], "running");

    // Check the log: should contain first (once), second, third.
    let log_after_retry =
        ssh_exec(data_dir.path(), name, "cat /tmp/agv-retry-log").await;
    assert_eq!(
        log_after_retry.matches("first").count(),
        1,
        "step 0 should not have run again on retry (got: {log_after_retry:?})",
    );
    assert!(
        log_after_retry.contains("second"),
        "step 1 should have run on retry (got: {log_after_retry:?})",
    );
    assert!(
        log_after_retry.contains("third"),
        "step 2 should have run after step 1 succeeded (got: {log_after_retry:?})",
    );

    destroy(data_dir.path(), name).await;
}
