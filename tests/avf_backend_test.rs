//! Integration tests for `LocalAvfBackend` — the Rust-side API users
//! actually hit when running `agv create --backend avf`, `agv suspend`,
//! `agv resume`, `agv stop`.
//!
//! These tests drive the backend trait directly (via `agv::vm::backend::
//! for_config`), which means they exercise the same code paths the
//! lifecycle dispatch in `src/vm/mod.rs` runs. The Swift binary is
//! spawned underneath, but the test never speaks JSON-RPC itself —
//! that's `LocalAvfBackend`'s job. This is the layer we ship.
//!
//! Runtime-skip:
//! - On non-macOS hosts: skipped at compile time (the whole file is
//!   gated on `target_os = "macos"`).
//! - On macOS without the Swift runner built: skipped via runtime check
//!   for the binary at `swift/avf-runner/.build/release/agv-avf-runner`.
//!   Build with `just build-avf-runner`.
//! - On macOS without the cached PoC raw disk: skipped. Boot fixture is
//!   the same one the swift-binary tests use — see `tests/avf_runner_test.rs`.
//!
//! Marked `#[ignore]` because each test boots a real VM (~10-30s each).
//! Run with `cargo test -- --include-ignored --nocapture`.

#![cfg(target_os = "macos")]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agv::config::ResolvedConfig;
use agv::vm::backend;
use agv::vm::cloud_init;
use agv::vm::instance::Instance;
use serial_test::serial;

/// Locate the release build of the Swift runner. Returns None if it
/// hasn't been built — tests then skip.
fn runner_binary() -> Option<PathBuf> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let path =
        PathBuf::from(manifest_dir).join("swift/avf-runner/.build/release/agv-avf-runner");
    if path.exists() { Some(path) } else { None }
}

/// The base raw disk produced by the qcow2-rs PoC. Required for the
/// boot tests since `provision_disk` needs a real qcow2 source — but
/// these tests skip that step and inject the raw directly, since the
/// converter has its own test coverage. Returns None if absent.
fn cached_raw() -> Option<PathBuf> {
    let p = PathBuf::from(
        "/tmp/qcow2-poc/out/debian-12-genericcloud-arm64-20260210-2384.ours.raw",
    );
    if p.exists() { Some(p) } else { None }
}

/// Build a minimal `Instance` + a `ResolvedConfig` pointing at the AVF
/// backend, with all the disk/seed/EFI/snapshot paths laid out the way
/// `LocalAvfBackend` expects them. Also generates a real cloud-init
/// seed ISO so the guest's hostname matches `name` (required for the
/// runner's lease-lookup-by-hostname).
async fn setup(dir: &std::path::Path, name: &str) -> Instance {
    let inst = Instance {
        name: name.to_string(),
        dir: dir.to_path_buf(),
    };

    // Copy the cached raw disk to the AVF disk path. `provision_disk`
    // would normally do the qcow2→raw conversion + resize, but we
    // skip it: that path has its own test coverage and adds ~30s
    // (decode) per test we don't want to pay here.
    let raw = cached_raw().expect("cached raw should exist (call cached_raw().is_some() first)");
    tokio::fs::copy(&raw, inst.avf_disk_path())
        .await
        .expect("copy cached raw");

    // Real cloud-init seed — the runner's status RPC looks up the
    // guest IP by hostname (via /var/db/dhcpd_leases on macOS), so
    // the seed has to set local-hostname to the VM name. A dummy
    // SSH key keeps the cloud-init schema happy.
    cloud_init::generate_seed(
        &inst.seed_path(),
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITestKeyForAvfBackendTest test@agv",
        name,
        "agent",
    )
    .await
    .expect("generate seed");

    inst
}

/// A `ResolvedConfig` with `backend = "avf"` and the smallest VM that
/// AVF will accept (1 GiB / 2 vCPU). Memory matches the runner's
/// validate-only path and what the manual test exercises; small enough
/// that suspend's snapshot save is fast.
fn avf_config() -> ResolvedConfig {
    ResolvedConfig {
        base_url: String::new(),
        base_checksum: String::new(),
        skip_checksum: true,
        memory: "1G".to_string(),
        cpus: 2,
        disk: "3G".to_string(),
        user: "agent".to_string(),
        os_family: "debian".to_string(),
        files: vec![],
        setup: vec![],
        provision: vec![],
        forwards: vec![],
        auto_forwards: BTreeMap::new(),
        template_name: None,
        mixins_applied: vec![],
        mixin_notes: vec![],
        config_notes: vec![],
        mixin_manual_steps: vec![],
        config_manual_steps: vec![],
        labels: BTreeMap::new(),
        idle_suspend_minutes: 0,
        idle_load_threshold: 0.2,
        machine_type: None,
        backend: "avf".to_string(),
    }
}

/// Wait for the backend's `ssh_endpoint` to return a guest IP. Used as
/// a proxy for "VM has booted and DHCP completed" — the same signal
/// the production `wait_for_ssh` path uses.
async fn wait_for_guest_ip(
    backend: &dyn backend::VmBackend,
    inst: &Instance,
    timeout: Duration,
) -> anyhow::Result<String> {
    let deadline = std::time::Instant::now() + timeout;
    let mut last_err: Option<anyhow::Error> = None;
    while std::time::Instant::now() < deadline {
        match backend.ssh_endpoint(inst).await {
            Ok((ip, _)) => return Ok(ip),
            Err(e) => last_err = Some(e),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(anyhow::anyhow!(
        "guest IP didn't appear within {timeout:?}; last error: {:?}",
        last_err
    ))
}

/// Build a per-run unique VM name. Avoids stale entries in
/// `/var/db/dhcpd_leases` from earlier runs masking real DHCP
/// completion — the runner's `status` op returns whatever IP is in
/// the lease file for the hostname, and a stale entry would let
/// `wait_for_guest_ip` return before the VM has actually booted.
fn unique_name(prefix: &str) -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}-{ts}")
}

/// Make `LocalAvfBackend::start`'s binary resolver find the Swift
/// runner without setting an env var. The resolver's fallback path
/// is "sibling of `std::env::current_exe()`" — and in a Cargo test
/// `current_exe()` is the per-test binary under `target/.../deps/`.
/// So we symlink the Swift runner to `agv-avf-runner` alongside the
/// test binary, and the production resolver picks it up.
///
/// Symlink, not copy: macOS's AppleSystemPolicy provenance sandbox
/// rejects the code signature when a signed Mach-O is copied to a
/// new path (`load code signature error 2`); the runner gets
/// SIGKILL'd before producing any output. A symlink avoids that by
/// keeping the kernel pointed at the originally-signed bytes.
///
/// This also avoids the `unsafe { set_var }` route — the crate is
/// `unsafe_code = "forbid"` so that's not available even in tests.
fn ensure_runner_alongside_test_binary(source: &std::path::Path) {
    let dest = std::env::current_exe()
        .expect("current_exe")
        .parent()
        .expect("test binary has a parent dir")
        .join("agv-avf-runner");
    // Idempotent: if the symlink already points at the right place, no-op.
    if let Ok(cur) = std::fs::read_link(&dest) {
        if cur == source {
            return;
        }
    }
    let _ = std::fs::remove_file(&dest);
    std::os::unix::fs::symlink(source, &dest)
        .expect("symlink runner alongside test binary");
}

/// End-to-end: cold boot → suspend → resume through the Rust API the
/// production lifecycle dispatch uses. This closes the test gap where
/// the Swift-binary suspend test was marked `#[should_panic]` for the
/// known harness flake.
///
/// Steps:
///   1. setup() — copy cached raw, gen seed
///   2. backend.start(loadvm=None) — cold boot, polls runner to running
///   3. wait_for_guest_ip — proxy for "guest finished cloud-init DHCP"
///   4. backend.suspend(inst) — sends suspend RPC, waits for runner
///      exit, sanity-checks snapshot file
///   5. assert snapshot file exists
///   6. backend.start(loadvm=Some("agv-suspend")) — resume from snapshot
///   7. wait_for_guest_ip again — VM is actually running
///   8. assert snapshot file is gone (runner removes after successful
///      resume)
///   9. backend.stop(inst) — clean shutdown
#[tokio::test]
#[ignore = "boots a real Apple Virtualization VM via the Rust backend API — slow"]
#[serial]
async fn cold_boot_suspend_resume_round_trip() {
    let Some(runner) = runner_binary() else {
        eprintln!("agv-avf-runner not built — skipping cold_boot_suspend_resume_round_trip");
        return;
    };
    if cached_raw().is_none() {
        eprintln!("cached raw disk not present — skipping cold_boot_suspend_resume_round_trip");
        return;
    }
    ensure_runner_alongside_test_binary(&runner);

    let dir = tempfile::tempdir().unwrap();
    let name = unique_name("avf-backend-rt");
    let inst = setup(dir.path(), &name).await;
    let cfg = avf_config();
    let backend = backend::for_config(&cfg);

    // --- Cold boot ---
    // `start` polls until the runner reports state=running. The
    // machine_type arg is ignored by AVF.
    backend
        .start(&inst, &cfg, "ignored-for-avf", None)
        .await
        .expect("cold boot start");
    let ip = wait_for_guest_ip(backend, &inst, Duration::from_secs(90))
        .await
        .expect("guest IP after cold boot");
    assert!(!ip.is_empty(), "guest IP should not be empty");
    assert!(
        inst.avf_runner_pid_path().exists(),
        "runner PID file should be written"
    );

    // --- Suspend ---
    // Sends the suspend RPC, waits up to 60s for the runner to exit
    // (saveMachineStateTo finishes), then sanity-checks the snapshot
    // and removes the PID file.
    backend.suspend(&inst).await.expect("suspend");
    assert!(
        inst.avf_snapshot_path().exists(),
        "snapshot file should exist after suspend at {}",
        inst.avf_snapshot_path().display()
    );
    assert!(
        !inst.avf_runner_pid_path().exists(),
        "runner PID file should be cleaned up after suspend"
    );
    let snap_size = std::fs::metadata(inst.avf_snapshot_path())
        .unwrap()
        .len();
    // Snapshot for a 1 GiB VM is typically 50-200 MiB; require >1 MiB
    // as a sanity floor.
    assert!(
        snap_size > 1024 * 1024,
        "snapshot file is suspiciously small: {snap_size} bytes"
    );

    // --- Resume ---
    // `loadvm`'s value is ignored by AVF (one snapshot per VM); only
    // `is_some()` matters. The runner will `restoreMachineStateFrom`
    // + `vm.resume`, then poll until status=running.
    let resume_log = inst.dir.join("avf-runner.log");
    if let Err(e) = backend
        .start(&inst, &cfg, "ignored-for-avf", Some("agv-suspend"))
        .await
    {
        let log = std::fs::read_to_string(&resume_log).unwrap_or_default();
        panic!(
            "resume start failed: {e:#}\n\n--- runner log ({}) ---\n{log}",
            resume_log.display()
        );
    }
    let ip2 = wait_for_guest_ip(backend, &inst, Duration::from_secs(60))
        .await
        .expect("guest IP after resume");
    assert!(!ip2.is_empty(), "guest IP should not be empty after resume");
    assert!(
        !inst.avf_snapshot_path().exists(),
        "snapshot file should be cleaned up after successful resume"
    );

    // --- Stop ---
    backend.stop(&inst).await.expect("stop");
    assert!(
        !inst.avf_runner_pid_path().exists(),
        "runner PID file should be cleaned up after stop"
    );
}

/// Smaller scope: cold boot → suspend only. Useful as a fast-fail when
/// debugging — exercises the suspend write path without the resume
/// roundtrip, which depends on more of the Swift state machine.
#[tokio::test]
#[ignore = "boots a real Apple Virtualization VM via the Rust backend API — slow"]
#[serial]
async fn cold_boot_then_suspend_writes_snapshot() {
    let Some(runner) = runner_binary() else {
        eprintln!("agv-avf-runner not built — skipping");
        return;
    };
    if cached_raw().is_none() {
        eprintln!("cached raw disk not present — skipping");
        return;
    }
    ensure_runner_alongside_test_binary(&runner);

    let dir = tempfile::tempdir().unwrap();
    let name = unique_name("avf-backend-susp");
    let inst = setup(dir.path(), &name).await;
    let cfg = avf_config();
    let backend = backend::for_config(&cfg);

    backend
        .start(&inst, &cfg, "ignored", None)
        .await
        .expect("cold boot start");
    wait_for_guest_ip(backend, &inst, Duration::from_secs(90))
        .await
        .expect("guest IP after cold boot");

    // Capture the runner log path before suspend so we can dump it
    // on failure — the test's tempdir would otherwise be GCed and
    // we'd lose the trace.
    let log_path = inst.dir.join("avf-runner.log");
    if let Err(e) = backend.suspend(&inst).await {
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        panic!(
            "suspend failed: {e:#}\n\n--- runner log ({}) ---\n{log}",
            log_path.display()
        );
    }
    assert!(
        inst.avf_snapshot_path().exists(),
        "snapshot file should exist after suspend"
    );
    assert!(
        !inst.avf_runner_pid_path().exists(),
        "runner PID file should be cleaned up after suspend"
    );
}
