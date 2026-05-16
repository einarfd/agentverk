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
//! - On macOS without the cached `PoC` raw disk: skipped. Boot fixture is
//!   the same one the swift-binary tests use — see `tests/avf_runner_test.rs`.
//!
//! Marked `#[ignore]` because each test boots a real VM (~10-30s each).
//! Run with `cargo test -- --include-ignored --nocapture`.

#![cfg(target_os = "macos")]
// Test-pragmatism lints. macOS-only file; pinned at file scope per
// the AGENTS.md convention (use `#[expect]` over `#[allow]`).
#![expect(
    clippy::doc_markdown,
    reason = "test docstrings quote tool / path names freely without backticks"
)]
#![expect(
    clippy::map_unwrap_or,
    reason = "the read .map().unwrap_or() form reads as 'normalize-or-default' more clearly than .map_or() in test setup"
)]
#![expect(
    clippy::uninlined_format_args,
    reason = "older format-arg style kept for parity with surrounding test helpers"
)]

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

/// The base raw disk used as the fixture for the boot tests. These
/// tests skip `provision_disk` (which would handle the qcow2→raw
/// conversion + resize) and inject the raw directly — the converter
/// has its own test coverage and adds ~30s per test we don't want to
/// pay here.
///
/// Checks two locations in order:
///   1. `~/.local/share/agv/cache/images/…qcow2.raw` — agv's raw cache,
///      populated by any prior `agv create --backend avf`. This is the
///      durable location.
///   2. `/tmp/qcow2-poc/out/…ours.raw` — legacy path from the qcow2-rs
///      PoC. Kept as a fallback for developers with that workspace,
///      but unreliable on macOS where `/tmp` is swept on day-boundary
///      reboots.
fn cached_raw() -> Option<PathBuf> {
    let candidates = [
        std::env::home_dir().map(|h| {
            h.join(".local/share/agv/cache/images")
                .join("debian-12-genericcloud-arm64-20260210-2384.qcow2.raw")
        }),
        Some(PathBuf::from(
            "/tmp/qcow2-poc/out/debian-12-genericcloud-arm64-20260210-2384.ours.raw",
        )),
    ];
    candidates.into_iter().flatten().find(|p| p.exists())
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

/// Cold boot → backend.suspend() refusal contract test.
///
/// Apple Virtualization framework does not support save/restore for
/// Linux guests as of macOS 26 (Tahoe and earlier). The runner refuses
/// the `suspend` op with a clear error, and `LocalAvfBackend::suspend`
/// surfaces that refusal as an `anyhow::Error` containing the
/// framework-limitation message. This test exercises the Rust backend
/// path (one layer below the CLI; one layer above the runner RPC).
///
/// If/when Apple lifts the restriction, this test will fail and we'll
/// rewrite it as the full suspend/resume round-trip we originally had.
///
/// Steps:
///   1. setup() — copy cached raw, gen seed
///   2. backend.start(loadvm=None) — cold boot, polls runner to running
///   3. wait_for_guest_ip — proxy for "guest finished cloud-init DHCP"
///   4. backend.suspend(inst) → assert Err with the framework message,
///      no snapshot written, runner PID file intact (VM still alive)
///   5. backend.stop(inst) — clean shutdown
#[tokio::test]
#[ignore = "boots a real Apple Virtualization VM via the Rust backend API — slow"]
#[serial]
async fn cold_boot_suspend_refused_then_stop() {
    let Some(runner) = runner_binary() else {
        eprintln!("agv-avf-runner not built — skipping cold_boot_suspend_refused_then_stop");
        return;
    };
    if cached_raw().is_none() {
        eprintln!("cached raw disk not present — skipping cold_boot_suspend_refused_then_stop");
        return;
    }
    ensure_runner_alongside_test_binary(&runner);

    let dir = tempfile::tempdir().unwrap();
    let name = unique_name("avf-backend-rf");
    let inst = setup(dir.path(), &name).await;
    let cfg = avf_config();
    let backend = backend::for_config(&cfg);

    // --- Cold boot ---
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

    // --- Suspend RPC must surface the framework-limitation refusal ---
    let err = backend
        .suspend(&inst)
        .await
        .expect_err("backend.suspend() must fail for an AVF Linux VM");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("Apple Virtualization framework") || msg.contains("save/restore"),
        "backend.suspend() error must mention the framework limitation; got: {msg}"
    );
    // Refusal must not leave a misleading snapshot file behind.
    assert!(
        !inst.avf_snapshot_path().exists(),
        "refused backend.suspend() must not have written a snapshot file"
    );
    // The runner must still be alive — refusal happens at the RPC
    // boundary, the VM keeps running. PID file is the cheapest proof.
    assert!(
        inst.avf_runner_pid_path().exists(),
        "runner PID file must persist after a refused suspend (VM still alive)"
    );

    // --- Stop cleanly ---
    backend.stop(&inst).await.expect("stop");
    assert!(
        !inst.avf_runner_pid_path().exists(),
        "runner PID file should be cleaned up after stop"
    );
}
