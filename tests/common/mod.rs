//! Shared helpers for the integration-test suite.
//!
//! Rust's integration-test layout treats each `tests/*.rs` as its
//! own crate, so this module is included by every test file via
//! `mod common;`. The `#![allow(dead_code)]` is necessary because
//! not every test file uses every helper. `#[expect]` would be
//! preferred per project convention, but it doesn't fit here —
//! dead-code fires per-crate, so any test file that happens to use
//! every helper would see `#[expect(dead_code)]` as unfulfilled.
//! `#[allow]` with a reason is the correct pattern for this shape.

#![allow(
    dead_code,
    reason = "helpers are included by every test crate via `mod common;` but used selectively"
)]

use std::path::Path;

use tempfile::TempDir;

/// Build a fresh `AGV_DATA_DIR`-shaped tempdir for an integration
/// test, sharing the image cache with the user's real
/// `~/.local/share/agv/cache/images/` via a symlink.
///
/// **Why a symlink.** The slow boot tests previously called
/// `tempfile::tempdir()` directly. agv resolves its cache at
/// `<AGV_DATA_DIR>/cache/images/`, so a fresh tempdir meant a fresh
/// (empty) cache — every test re-downloaded the ~330 MiB cloud
/// image into its own tempdir. Across one `--include-ignored` run
/// of all the slow boot tests that's gigabytes of redundant
/// downloads, several minutes of test time, and a hard dependency
/// on the host's network not blipping mid-stream (which has bitten
/// us in practice — see the per-chunk-stall timeout in
/// `src/image.rs`).
///
/// A symlink to the user's real cache means:
/// 1. Tests skip re-downloading if the image is already cached
///    (the common case once the user has created any AVF VM).
/// 2. If the image isn't cached, the first slow test downloads it
///    into the user's real cache via the symlink; subsequent tests
///    on the same machine see it instantly.
/// 3. Test cleanup (`TempDir::drop`) removes the symlink, never
///    the target — the user's cache is untouched.
/// 4. Works the same on macOS and Linux. No filesystem-specific
///    primitives (`clonefile`, `reflink`) needed.
///
/// **Caveats.** The image cache is read-only-after-download by
/// agv's convention, so sharing is safe. A user who manually deletes
/// the cached image mid-test would see the symlink dangle, but that
/// would surface as a clear "file not found" error and isn't a
/// realistic scenario. If `~/.local/share/agv/cache/images/` doesn't
/// exist yet (fresh host), the symlink is skipped and agv populates
/// a real cache dir inside the tempdir as before.
/// **Why not plain `tempfile::tempdir()`.** A VM's QMP socket lives at
/// `<data_dir>/instances/<name>/qmp.sock`, and a unix socket path has a
/// hard limit of `sizeof(sun_path)` — 104 bytes on macOS, so 103 usable.
/// macOS hands out `TMPDIR=/var/folders/<...>/T`, 48 characters before
/// anything of ours, which leaves 24 for the VM name. Several test VMs
/// are longer than that (`_test-auto-suspend-active` is 25), and the
/// failure is thoroughly unobvious: QEMU binds an over-long path anyway,
/// then nothing can connect to it. Anchoring at `/tmp` raises the budget
/// to 68.
#[must_use]
pub fn test_data_dir() -> TempDir {
    let short_base = Path::new("/tmp");
    let dir = if short_base.is_dir() {
        tempfile::Builder::new()
            .prefix("agv")
            .tempdir_in(short_base)
            .expect("create test tempdir")
    } else {
        tempfile::tempdir().expect("create test tempdir")
    };
    share_image_cache(dir.path());
    dir
}

/// Implementation of the symlink, best-effort. Every failure path
/// (no home dir, no user cache, mkdir fails, symlink fails) falls
/// back silently to "no optimization, agv downloads normally" — the
/// shared cache is an accelerant, never a correctness requirement.
fn share_image_cache(test_data_dir: &Path) {
    let Some(home) = std::env::home_dir() else { return };
    let user_cache = home.join(".local/share/agv/cache/images");
    if !user_cache.is_dir() {
        return;
    }
    let test_cache_parent = test_data_dir.join("cache");
    if std::fs::create_dir_all(&test_cache_parent).is_err() {
        return;
    }
    let test_cache_images = test_cache_parent.join("images");
    let _ = std::os::unix::fs::symlink(&user_cache, &test_cache_images);
}
