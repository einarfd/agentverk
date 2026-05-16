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

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serial_test::serial;

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

/// Locate a bootable raw debian-12 disk to use as a fixture for the
/// boot-the-VM slow tests. Returns `None` if no fixture exists —
/// tests skip in that case.
///
/// Checks two paths in order:
///   1. `~/.local/share/agv/cache/images/debian-12-…qcow2.raw` —
///      the agv raw cache (populated automatically the first time
///      any AVF VM is created from this base; see `src/raw_cache.rs`).
///   2. `/tmp/qcow2-poc/out/debian-12-…ours.raw` — legacy path from
///      the qcow2-rs proof-of-concept. Kept as a fallback because
///      `/tmp` gets swept on macOS day-boundary reboots and the
///      cache path is the more durable home now.
///
/// Both files are byte-identical for the converted region (the
/// agv cache is produced by the same qcow2-rs code path) so the
/// tests behave identically regardless of which one is found.
fn bootable_raw_fixture() -> Option<PathBuf> {
    let candidates = [
        std::env::home_dir().map(|h| {
            h.join(".local/share/agv/cache/images")
                .join("debian-12-genericcloud-arm64-20260210-2384.qcow2.raw")
        }),
        Some(PathBuf::from(
            "/tmp/qcow2-poc/out/debian-12-genericcloud-arm64-20260210-2384.ours.raw",
        )),
    ];
    candidates
        .into_iter()
        .flatten()
        .find(|p| p.exists())
}

/// Generate a tiny seed.iso via macOS's built-in `hdiutil`, matching
/// what the real lifecycle code does. The hostname is set to
/// `vm_name` so the lease lookup (which keys on the hostname bootpd
/// records) can find this VM after DHCP.
fn make_seed_iso(dir: &std::path::Path, vm_name: &str) -> PathBuf {
    let seed_src = dir.join("seed-src");
    std::fs::create_dir_all(&seed_src).unwrap();
    std::fs::write(
        seed_src.join("meta-data"),
        format!("instance-id: {vm_name}\nlocal-hostname: {vm_name}\n"),
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

/// Wire-protocol version currently expected by the runner. Must
/// match `RUNNER_PROTOCOL_VERSION` in
/// `src/vm/backend.rs` and the Swift constant in main.swift.
/// Tests pin the value explicitly rather than importing the Rust
/// constant — the runner is a black box from the test's
/// perspective, and the protocol version is part of its public
/// contract.
const RUNNER_PROTOCOL_VERSION: u32 = 1;

/// Write a runner config JSON pointing at the supplied disk + seed paths.
fn write_config(
    dir: &std::path::Path,
    name: &str,
    disk: &std::path::Path,
    seed: &std::path::Path,
) -> PathBuf {
    write_config_with_protocol_version(dir, name, disk, seed, RUNNER_PROTOCOL_VERSION)
}

/// Variant of [`write_config`] that lets the test choose the protocol
/// version. Used by the version-mismatch test to write a config the
/// runner must reject.
fn write_config_with_protocol_version(
    dir: &std::path::Path,
    name: &str,
    disk: &std::path::Path,
    seed: &std::path::Path,
    protocol_version: u32,
) -> PathBuf {
    let cfg = format!(
        r#"{{
  "runner_protocol_version": {protocol_version},
  "name": "{name}",
  "memory_bytes": 1073741824,
  "cpu_count": 2,
  "disk_path": "{disk}",
  "seed_iso_path": "{seed}",
  "efi_variable_store_path": "{efi}",
  "serial_log_path": "{serial}",
  "control_socket_path": "{ctl}",
  "snapshot_path": "{snap}",
  "restore_on_boot": false,
  "mac_address_path": "{mac}",
  "machine_identifier_path": "{mid}"
}}
"#,
        name = name,
        disk = disk.display(),
        seed = seed.display(),
        efi = dir.join("efi-vars.bin").display(),
        serial = dir.join("serial.log").display(),
        ctl = dir.join("control.sock").display(),
        snap = dir.join("avf-snapshot.bin").display(),
        mac = dir.join("avf-mac").display(),
        mid = dir.join("avf-machine-id").display(),
    );
    let path = dir.join("config.json");
    std::fs::write(&path, cfg).unwrap();
    path
}

#[test]
fn version_flag_reports_protocol_version() {
    let Some(binary) = runner_binary() else {
        eprintln!(
            "agv-avf-runner not built — skipping version_flag_reports_protocol_version (run: just build-avf-runner)"
        );
        return;
    };
    let out = Command::new(&binary).arg("--version").output().unwrap();
    assert!(out.status.success(), "--version exited non-zero");
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The runner prints `agv-avf-runner protocol <N>` so agv (or
    // a human reading `agv doctor` output) can sanity-check what
    // wire version the installed binary speaks.
    let expected = format!("agv-avf-runner protocol {RUNNER_PROTOCOL_VERSION}");
    assert!(
        stdout.trim() == expected,
        "expected `{expected}` from --version, got: {stdout:?}"
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
    let seed = make_seed_iso(dir.path(), "avf-test");
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
    let seed = make_seed_iso(dir.path(), "avf-test");
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

/// Wire-protocol version skew is fail-fast. Write a config that
/// declares a version the runner doesn't know and assert the runner
/// refuses with a clear, actionable error pointing at reinstall —
/// the install-skew scenario this guard exists to prevent (e.g.
/// `cargo install agv` upgraded the Rust side but the user's
/// `agv-avf-runner` is still from an older tarball).
#[test]
fn rejects_config_with_wrong_protocol_version() {
    let Some(binary) = runner_binary() else {
        eprintln!(
            "agv-avf-runner not built — skipping rejects_config_with_wrong_protocol_version"
        );
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let disk = make_empty_raw(dir.path());
    let seed = make_seed_iso(dir.path(), "avf-version-mismatch");
    // 999 stays far enough above the current version that this test
    // doesn't need to be updated every time we bump.
    let cfg = write_config_with_protocol_version(
        dir.path(),
        "avf-version-mismatch",
        &disk,
        &seed,
        999,
    );

    let out = Command::new(&binary)
        .arg("--config")
        .arg(&cfg)
        .arg("--validate-only")
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "runner must refuse a config with a wrong protocol version"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("protocol version mismatch")
            && stderr.contains("v999")
            && stderr.contains(&format!("v{RUNNER_PROTOCOL_VERSION}")),
        "error must name both versions; got: {stderr}"
    );
    assert!(
        stderr.contains("Reinstall") || stderr.contains("reinstall"),
        "error should point users at the reinstall fix; got: {stderr}"
    );
}

/// Slow boot test: spin up a real AVF VM from the cached Debian raw,
/// wait for the kernel to log to the serial pipe, then SIGTERM and
/// confirm graceful exit. Skipped unless the cached raw exists in the
/// `PoC` location (`/tmp/qcow2-poc/out/...`); reproduce by running
/// the qcow2-rs proof-of-concept converter once.
///
/// Marked `#[ignore]` because it boots a real VM (~5–10s), which is
/// the same cost as our slow QEMU boot tests.
// All slow boot tests run serially. They share the host's
// /var/db/dhcpd_leases, and because we boot from copies of the same
// disk image, every VM sends the same systemd-networkd RFC 4361
// client identifier (derived from /etc/machine-id) — bootpd treats
// them as one host and overwrites the single lease entry, so parallel
// runs interfere with hostname-keyed lease lookups. Production agv
// will need to regenerate machine-id per VM (TODO: cloud-init
// runcmd).
#[test]
#[ignore = "boots a real Apple Virtualization VM — slow"]
#[serial]
fn boot_and_sigterm_exits_cleanly() {
    let Some(binary) = runner_binary() else {
        eprintln!("agv-avf-runner not built — skipping boot_and_sigterm_exits_cleanly");
        return;
    };
    let Some(cached_raw) = bootable_raw_fixture() else {
        eprintln!(
            "no bootable raw fixture available — skipping boot_and_sigterm_exits_cleanly \
             (run any `agv create --backend avf` to populate the raw cache)"
        );
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("disk.raw");
    std::fs::copy(&cached_raw, &disk).unwrap();
    let seed = make_seed_iso(dir.path(), "avf-boot-test");
    let cfg = write_config(dir.path(), "avf-boot-test", &disk, &seed);

    let mut child = Command::new(&binary)
        .arg("--config")
        .arg(&cfg)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    // We can't assert on serial.log content here: AVF only exposes
    // virtio-console (`/dev/hvc0`), but the Debian cloud kernel is
    // built with `console=ttyAMA0` baked into its GRUB config. Under
    // QEMU that's the PL011 UART (works); under AVF that device
    // doesn't exist, so the kernel logs go to /dev/null from our
    // perspective. Wiring `console=hvc0` requires modifying the disk
    // image's GRUB config at create time or shipping our own
    // bootloader; tracked for a follow-up commit.
    //
    // Wait for the runner to bind its socket and report state=running
    // (`VZVirtualMachine.start` returned). We then add a small blind
    // grace so the guest kernel has time to load its ACPI driver
    // before we signal — without it, requestStop falls through to
    // force-stop because the guest can't respond. The 5s is small
    // enough that a fully-booted guest's heavy services aren't yet
    // running, which keeps the SIGTERM shutdown path fast.
    //
    // This is the one blind wait we can't replace with polling: there
    // is no observable host-side signal between `state=running` (too
    // early — kernel may not be up) and `guest_ip` populated (too
    // late — full systemd, slow shutdown). See `ACPI_READY_GRACE`
    // docs.
    let socket_path = dir.path().join("control.sock");
    wait_for_socket_bound(&socket_path);
    wait_for_state_running(&socket_path);
    std::thread::sleep(ACPI_READY_GRACE);

    // SIGTERM should trigger the runner's requestStop path. The
    // kernel ACPIs through, the VM stops, the process exits.
    let pid = child.id();
    let kill = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("kill -TERM failed to spawn");
    assert!(kill.success(), "kill -TERM failed");

    let status = wait_for_child_exit(
        &mut child,
        "agv-avf-runner SIGTERM shutdown",
        SIGTERM_EXIT_DEADLINE,
    );
    assert!(
        status.success(),
        "agv-avf-runner should exit cleanly after SIGTERM, got {status:?}"
    );
}

/// Slow boot test: drive the runner via the JSON-RPC control socket.
/// Boots, queries `status` (asserts state == "running"), then sends
/// `stop`, asserts the response is `{ok: true}`, and finally waits for
/// the runner to exit cleanly.
///
/// This is the primary acceptance test for the control protocol — the
/// shape Rust clients will use in production.
#[test]
#[ignore = "boots a real Apple Virtualization VM — slow"]
#[serial]
fn control_socket_status_then_stop() {
    let Some(binary) = runner_binary() else {
        eprintln!("agv-avf-runner not built — skipping control_socket_status_then_stop");
        return;
    };
    let Some(cached_raw) = bootable_raw_fixture() else {
        eprintln!(
            "no bootable raw fixture available — skipping control_socket_status_then_stop \
             (run any `agv create --backend avf` to populate the raw cache)"
        );
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("disk.raw");
    std::fs::copy(&cached_raw, &disk).unwrap();
    let seed = make_seed_iso(dir.path(), "avf-control-test");
    let cfg = write_config(dir.path(), "avf-control-test", &disk, &seed);
    let socket_path = dir.path().join("control.sock");

    let mut child = Command::new(&binary)
        .arg("--config")
        .arg(&cfg)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    wait_for_socket_bound(&socket_path);
    wait_for_state_running(&socket_path);
    // Why we wait for guest_ip specifically (not just state=running):
    //   1. systemd-networkd does the first DHCP request before
    //      cloud-init has applied `local-hostname`, so the initial
    //      lease entry has a default ("debian") hostname.
    //   2. cloud-init reaches `cc_update_hostname` only in the init
    //      stage after networking, then triggers a DHCP renew.
    //   3. The renew is what writes our expected hostname into
    //      /var/db/dhcpd_leases, which our lookup keys on.
    // Warm boots complete the chain in 5-10s; cold boots can take
    // 30-60s. `wait_for_guest_ip` returns as soon as it fires.
    //
    // Swift's JSONEncoder omits nil Optionals from output, so the
    // response either contains `"guest_ip":"<ip>"` (lease found) or
    // no guest_ip key at all (still pending). Helper matches the
    // populated form.
    let last_status = wait_for_guest_ip(&socket_path);
    assert!(
        last_status.contains("\"ok\":true"),
        "status response should be ok=true: {last_status}"
    );
    assert!(
        last_status.contains("\"state\":\"running\""),
        "expected state=running in: {last_status}"
    );
    // Sanity: extract the IP and check it parses and is in a
    // private RFC 1918 range (Apple's NAT picks subnets like
    // 192.168.64.0/24 or 192.168.205.0/24).
    let ip = extract_guest_ip(&last_status)
        .unwrap_or_else(|| panic!("could not extract guest_ip from: {last_status}"));
    let parsed: std::net::Ipv4Addr = ip
        .parse()
        .unwrap_or_else(|e| panic!("guest_ip {ip:?} doesn't parse: {e}"));
    assert!(
        parsed.is_private(),
        "guest_ip {ip} should be in an RFC1918 range"
    );

    // Send stop and confirm the runner accepts it.
    let stop = jsonrpc(&socket_path, r#"{"op":"stop"}"#);
    assert!(
        stop.contains("\"ok\":true"),
        "stop response should be ok=true: {stop}"
    );

    // The graceful stop fires a guest ACPI shutdown; the runner
    // exits when guestDidStop fires. With the `.cached` + `.full`
    // disk-attachment mode the runner uses (needed to avoid ext4
    // corruption — see commit a1fe808), every guest write blocks on
    // host fsync, and systemd's shutdown does enough small writes
    // that graceful shutdown can take 5-10+ minutes under load.
    // That's longer than the test can usefully wait, and it's a
    // host-side I/O scheduling problem, not a runner-protocol bug.
    //
    // Mirror what `agv stop` does in production: give graceful a
    // short window (60s — covers a healthy unloaded shutdown), then
    // fall back to `force_stop` if it hasn't exited. Either path
    // proves the RPC contract — graceful path is the happy case;
    // force-stop fallback proves the runner stays controllable when
    // the guest's own shutdown is slow.
    let graceful_window = Duration::from_secs(60);
    let mut graceful_status = None;
    let start = std::time::Instant::now();
    while start.elapsed() < graceful_window {
        if let Some(s) = child.try_wait().expect("try_wait failed") {
            graceful_status = Some(s);
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    let final_status = if let Some(s) = graceful_status {
        eprintln!("graceful stop completed in {:?}", start.elapsed());
        s
    } else {
        eprintln!(
            "graceful stop didn't complete in {graceful_window:?}; falling back to force_stop \
             (expected under `.cached`+`.full` disk attachment on busy hosts)"
        );
        let fs = jsonrpc(&socket_path, r#"{"op":"force_stop"}"#);
        assert!(
            fs.contains("\"ok\":true"),
            "force_stop fallback should return ok=true: {fs}"
        );
        wait_for_child_exit(&mut child, "runner force_stop fallback", FORCE_STOP_EXIT_DEADLINE)
    };
    assert!(
        final_status.success(),
        "runner should exit cleanly (graceful or force-stop), got {final_status:?}"
    );

    // Socket file should be gone after the runner exits.
    assert!(
        !socket_path.exists(),
        "control socket file should be removed on shutdown"
    );
}

#[test]
#[ignore = "boots a real Apple Virtualization VM — slow"]
#[serial]
fn control_socket_unknown_op_returns_error() {
    let Some(binary) = runner_binary() else {
        eprintln!("agv-avf-runner not built — skipping control_socket_unknown_op_returns_error");
        return;
    };
    let Some(cached_raw) = bootable_raw_fixture() else {
        eprintln!(
            "no bootable raw fixture available — skipping control_socket_unknown_op_returns_error \
             (run any `agv create --backend avf` to populate the raw cache)"
        );
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("disk.raw");
    std::fs::copy(&cached_raw, &disk).unwrap();
    let seed = make_seed_iso(dir.path(), "avf-control-err-test");
    let cfg = write_config(dir.path(), "avf-control-err-test", &disk, &seed);
    let socket_path = dir.path().join("control.sock");

    let mut child = Command::new(&binary)
        .arg("--config")
        .arg(&cfg)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    wait_for_socket_bound(&socket_path);
    // Unknown-op dispatch doesn't need the guest to be reachable —
    // we only need the runner's RPC handler alive. `state=running`
    // is sufficient and lands in <5s.
    wait_for_state_running(&socket_path);

    let resp = jsonrpc(&socket_path, r#"{"op":"bogus"}"#);
    assert!(
        resp.contains("\"ok\":false"),
        "unknown op should return ok=false: {resp}"
    );
    assert!(
        resp.contains("unknown op"),
        "expected 'unknown op' in error: {resp}"
    );

    // Cleanup: `force_stop` bypasses guest ACPI (which may not be
    // wired up yet — we only waited for state=running) and exits
    // within a couple of seconds. This test is exercising the RPC
    // error path, not graceful shutdown — the `stop` op is covered
    // by `control_socket_status_then_stop`.
    let _ = jsonrpc(&socket_path, r#"{"op":"force_stop"}"#);
    wait_for_child_exit(
        &mut child,
        "runner force_stop (unknown-op test cleanup)",
        FORCE_STOP_EXIT_DEADLINE,
    );
}

/// AVF suspend refusal contract test.
///
/// Apple Virtualization framework does not support save/restore for
/// Linux guests as of macOS 15 / 26: `saveMachineStateTo` succeeds but
/// `restoreMachineStateFrom` consistently fails with the misleading
/// `VZErrorDomain Code=12 "permission denied"`, regardless of process
/// boundary or device list. Reproduced exhaustively (canonicalized
/// paths via `realpath(3)`, minimal device list, persisted MAC +
/// machine identifier, validated by `validateSaveRestoreSupport()`
/// which optimistically returns ok). Apple's own sample code and every
/// working public project (Tart, UTM, Lima) restrict save/restore to
/// macOS guests.
///
/// Rather than write an apparently-working snapshot file that can't be
/// restored, the runner refuses the `suspend` op up front with a
/// clear, actionable error. This test locks in that contract: if the
/// framework ever lifts the restriction and we re-enable the path,
/// this test will fail and we'll know to write the real round-trip
/// test.
#[test]
#[ignore = "boots a real Apple Virtualization VM — slow"]
#[serial]
fn suspend_rpc_refuses_until_framework_supports_linux() {
    let Some(binary) = runner_binary() else {
        eprintln!("agv-avf-runner not built — skipping suspend_rpc_refuses_until_framework_supports_linux");
        return;
    };
    let Some(cached_raw) = bootable_raw_fixture() else {
        eprintln!(
            "no bootable raw fixture available — skipping suspend_rpc_refuses_until_framework_supports_linux \
             (run any `agv create --backend avf` to populate the raw cache)"
        );
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("disk.raw");
    std::fs::copy(&cached_raw, &disk).unwrap();
    let seed = make_seed_iso(dir.path(), "avf-suspend-refuse-test");
    let cfg = write_config(dir.path(), "avf-suspend-refuse-test", &disk, &seed);
    let socket_path = dir.path().join("control.sock");
    let snapshot_path = dir.path().join("avf-snapshot.bin");

    let mut child = Command::new(&binary)
        .arg("--config")
        .arg(&cfg)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    wait_for_socket_bound(&socket_path);
    wait_for_state_running(&socket_path);

    let suspend = jsonrpc(&socket_path, r#"{"op":"suspend"}"#);
    assert!(
        suspend.contains("\"ok\":false"),
        "AVF suspend must be refused — Linux save/restore is unsupported by the framework. Got: {suspend}"
    );
    assert!(
        suspend.contains("Apple Virtualization framework"),
        "refusal should mention the framework limitation: {suspend}"
    );
    assert!(
        suspend.contains("qemu backend") || suspend.contains("agv stop"),
        "refusal should point users at workarounds: {suspend}"
    );
    // Critical: refusal must not leave a misleading snapshot file
    // on disk. If the file exists, callers might think suspend
    // worked and attempt a resume that would fail with Code=12.
    assert!(
        !snapshot_path.exists(),
        "refused suspend should not have written a snapshot file at {}",
        snapshot_path.display()
    );

    // Cleanup: force_stop for predictable exit timing — this test
    // exercised the suspend RPC error path, not graceful shutdown.
    let _ = jsonrpc(&socket_path, r#"{"op":"force_stop"}"#);
    wait_for_child_exit(
        &mut child,
        "runner force_stop (suspend-refusal test cleanup)",
        FORCE_STOP_EXIT_DEADLINE,
    );
}

// ---------------------------------------------------------------------------
// Test helpers for the control socket.
// ---------------------------------------------------------------------------

// Per-phase deadlines. Each one is the wall-clock ceiling for a single
// observable transition (socket bound, guest state==running, child
// exited, etc.), so the *cumulative* test runtime is the sum of these.
// No blind sleeps left in any slow test — every wait polls on the
// actual condition with a clear timeout error if it never fires.

/// agv-avf-runner binds its control socket before calling `vm.start`,
/// so the socket appears within a few hundred ms on a healthy host.
/// 30s gives back-to-back boot tests room under `verify-slow` load.
const SOCKET_BIND_DEADLINE: Duration = Duration::from_secs(30);

/// `vm.start` → state="running" is fast (<5s on a healthy host); 30s
/// covers a contended scheduler.
const STATE_RUNNING_DEADLINE: Duration = Duration::from_secs(30);

/// systemd-networkd's first DHCP request happens before cloud-init has
/// applied `local-hostname`; the renew that writes our expected
/// hostname into `/var/db/dhcpd_leases` lands once cloud-init reaches
/// `cc_update_hostname`. Warm boots: 5-10s. Cold boots after a fresh
/// EFI vars file: 30-60s. 90s is the documented cold-boot ceiling.
const GUEST_IP_DEADLINE: Duration = Duration::from_secs(90);

/// Runner exits after a SIGTERM-initiated graceful shutdown on a
/// guest that's at the `ACPI_READY_GRACE` sweet spot (ACPI loaded,
/// systemd not yet running heavy services). Kernel halts within a
/// few seconds; 60s is the failure ceiling, not the expected wait.
const SIGTERM_EXIT_DEADLINE: Duration = Duration::from_secs(60);


/// `force_stop` bypasses guest ACPI and stops the VM directly via
/// `VZVirtualMachine.stop`. Used for cleanup in tests that aren't
/// specifically exercising graceful shutdown — the runner exits
/// within a couple of seconds regardless of guest state. 30s is the
/// "something's deeply wrong" ceiling, not the expected wait.
const FORCE_STOP_EXIT_DEADLINE: Duration = Duration::from_secs(30);

/// Brief grace period for the guest kernel to reach the point where
/// its ACPI subsystem is wired up. The runner reports `state=running`
/// as soon as `VZVirtualMachine.start` returns — that's *before* the
/// guest kernel has loaded its ACPI driver. There's no observable
/// host-side signal between `state=running` (too early — ACPI not
/// loaded yet, requestStop hangs waiting on guestDidStop) and
/// `guest_ip` populated (much later — systemd services running,
/// graceful shutdown takes 60-120s). 8s lands the guest in the sweet
/// spot: ACPI driver loaded, but heavy services not yet started, so
/// graceful shutdown is fast.
///
/// This is the one blind wait the rewrite couldn't replace with
/// polling. Only used by tests that exercise the SIGTERM / graceful-
/// stop path; tests that don't care about graceful shutdown use the
/// `force_stop` RPC for cleanup instead.
const ACPI_READY_GRACE: Duration = Duration::from_secs(8);

/// Generic poll-and-wait helper. Calls `check()` repeatedly with
/// exponential backoff (capped at 500ms) until it returns `Some`, or
/// panics with a clear message if `deadline` elapses first. Use this
/// instead of `for _ in 0..N { sleep(); check() }` so the failure
/// message identifies *which* condition timed out instead of just
/// "test exceeded 5m".
fn wait_until<T, F>(label: &str, deadline: Duration, mut check: F) -> T
where
    F: FnMut() -> Option<T>,
{
    let start = std::time::Instant::now();
    let mut backoff = Duration::from_millis(50);
    loop {
        if let Some(value) = check() {
            return value;
        }
        if start.elapsed() >= deadline {
            panic!("{label}: condition never held after {deadline:?}");
        }
        std::thread::sleep(backoff);
        backoff = std::cmp::min(backoff * 2, Duration::from_millis(500));
    }
}

/// Wait for the runner's control socket file to exist. The runner
/// binds before `vm.start`, so this lands quickly; long deadlines
/// here only matter when the host is heavily loaded.
fn wait_for_socket_bound(path: &Path) {
    wait_until(
        &format!("control socket {} bound", path.display()),
        SOCKET_BIND_DEADLINE,
        || if path.exists() { Some(()) } else { None },
    );
}

/// Wait for the runner to report `state == "running"` via its `status`
/// RPC — i.e. `vm.start` has returned successfully. This is a strict
/// prerequisite for guest_ip to ever populate, so call it first.
///
/// Uses `try_jsonrpc` so a transient connect failure (socket file
/// briefly absent during state transition) is treated as "not ready
/// yet" and retried, not as a fatal error.
fn wait_for_state_running(socket_path: &Path) {
    wait_until("runner state=running", STATE_RUNNING_DEADLINE, || {
        let resp = try_jsonrpc(socket_path, r#"{"op":"status"}"#).ok()?;
        if resp.contains("\"state\":\"running\"") {
            Some(())
        } else {
            None
        }
    });
}

/// Wait until the guest has acquired a DHCP lease and the runner can
/// report a non-null `guest_ip`. This is the proxy we use for "guest
/// has booted far enough to react to ACPI shutdown / be suspended" —
/// by the time DHCP is up, systemd is far enough along that the
/// kernel + ACPI subsystem are wired.
///
/// Returns the captured status JSON so callers can extract the IP or
/// assert on other fields.
fn wait_for_guest_ip(socket_path: &Path) -> String {
    wait_until("guest_ip in status", GUEST_IP_DEADLINE, || {
        let resp = try_jsonrpc(socket_path, r#"{"op":"status"}"#).ok()?;
        if resp.contains("\"guest_ip\":\"") {
            Some(resp)
        } else {
            None
        }
    })
}

/// Wait for a spawned runner process to exit, returning its
/// `ExitStatus`. If `deadline` elapses, the child is killed (so the
/// test cleanup leaves no zombies behind) and the test panics with a
/// clear timeout message.
fn wait_for_child_exit(
    child: &mut std::process::Child,
    label: &str,
    deadline: Duration,
) -> std::process::ExitStatus {
    let start = std::time::Instant::now();
    let mut backoff = Duration::from_millis(50);
    loop {
        match child.try_wait().expect("try_wait failed") {
            Some(status) => return status,
            None => {
                if start.elapsed() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("{label}: process did not exit within {deadline:?}");
                }
                std::thread::sleep(backoff);
                backoff = std::cmp::min(backoff * 2, Duration::from_millis(500));
            }
        }
    }
}

/// Extract the `guest_ip` value from a JSON status response. Returns
/// `None` if the field is null or absent. Crude string-find rather
/// than a JSON parse so the test has no extra deps.
fn extract_guest_ip(json: &str) -> Option<String> {
    let needle = "\"guest_ip\":";
    let start = json.find(needle)? + needle.len();
    let rest = &json[start..];
    let rest = rest.trim_start();
    if rest.starts_with("null") {
        return None;
    }
    if !rest.starts_with('"') {
        return None;
    }
    let after_quote = &rest[1..];
    let end = after_quote.find('"')?;
    Some(after_quote[..end].to_string())
}

/// Send a single JSON-RPC line to the runner's control socket and
/// read the response. Returns the response line on success, or an
/// I/O error if the socket file is missing / connection refused /
/// read times out. Use this when polling — a transient connect
/// failure means "runner not ready yet", not "test failed".
///
/// The runner closes the connection after each response, so we
/// expect EOF after the newline.
fn try_jsonrpc(socket_path: &Path, request: &str) -> std::io::Result<String> {
    let mut stream = UnixStream::connect(socket_path)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(request.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    Ok(line.trim_end_matches('\n').to_string())
}

/// Strict variant: panics on any I/O error. Use only when the runner
/// is known to be alive (e.g. immediately after a `wait_*` helper
/// returned and we expect every subsequent RPC to land).
fn jsonrpc(socket_path: &Path, request: &str) -> String {
    try_jsonrpc(socket_path, request)
        .unwrap_or_else(|e| panic!("jsonrpc to {}: {e}", socket_path.display()))
}
