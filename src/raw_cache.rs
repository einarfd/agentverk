//! Cache of qcow2 → raw conversions for the AVF backend (macOS-only).
//!
//! Apple Virtualization can't read qcow2 directly; the
//! `provision_disk` step has to produce a raw disk file. Converting
//! a multi-GiB cloud image takes 5-30 seconds, and that cost used to
//! be paid once per VM created from that image. This module caches
//! the converted raw alongside the qcow2 in the image cache, so the
//! conversion only runs the first time a base image is used under
//! AVF; every later create reuses the cached raw via APFS
//! `clonefile(2)` (zero-copy until guest writes diverge).
//!
//! Filename scheme: `<basename>.qcow2` → `<basename>.qcow2.raw`,
//! e.g. `debian-12-generic-arm64.qcow2` →
//! `debian-12-generic-arm64.qcow2.raw`. Sibling files in the same
//! cache dir; `agv cache ls` / `agv cache clean` treat them as a
//! pair (see `image::referenced_cache_files`).
//!
//! Per-instance disks are independent of the cached raw after the
//! clone — APFS reference-counts the underlying extents, so deleting
//! either side never affects the other. The cache is purely a
//! speedup; instance disks are not bound to it.

#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _};
use tracing::{debug, info};

/// Derive the cached-raw path that corresponds to a qcow2 path.
/// Appends `.raw` to the qcow2 filename, keeping it as a sibling in
/// the same directory.
#[must_use]
pub fn cached_raw_path_for(qcow2_path: &Path) -> PathBuf {
    let mut s = qcow2_path.as_os_str().to_os_string();
    s.push(".raw");
    PathBuf::from(s)
}

/// Return the cached raw for this qcow2, converting first if missing.
///
/// Cross-process safe via the existing `locks::acquire_exclusive`
/// flock around `<raw>.lock` — two concurrent `agv create` calls
/// against the same base only convert once. Conversion writes to
/// `<raw>.partial` and atomically renames into place on success, so
/// a crashed converter never leaves a truncated raw the next run
/// would mistake for valid.
pub async fn ensure_cached_raw(qcow2_path: &Path) -> anyhow::Result<PathBuf> {
    let raw_path = cached_raw_path_for(qcow2_path);

    // Fast path: cached raw already exists, no lock needed.
    if tokio::fs::metadata(&raw_path).await.is_ok() {
        debug!(path = %raw_path.display(), "raw cache hit");
        return Ok(raw_path);
    }

    let mut lock_path = raw_path.clone();
    lock_path.as_mut_os_string().push(".lock");
    let _guard = crate::locks::acquire_exclusive(lock_path).await?;

    // Recheck after taking the lock — another agv process may have
    // converted while we were waiting on the flock.
    if tokio::fs::metadata(&raw_path).await.is_ok() {
        debug!(path = %raw_path.display(), "raw cache hit after lock");
        return Ok(raw_path);
    }

    let mut tmp_path = raw_path.clone();
    tmp_path.as_mut_os_string().push(".partial");

    info!(
        from = %qcow2_path.display(),
        to = %raw_path.display(),
        "populating raw cache (one-time conversion)"
    );
    crate::qcow2::convert_to_sparse_raw(qcow2_path, &tmp_path)
        .await
        .with_context(|| {
            format!(
                "converting {} to cached raw at {}",
                qcow2_path.display(),
                tmp_path.display(),
            )
        })?;

    tokio::fs::rename(&tmp_path, &raw_path)
        .await
        .with_context(|| {
            format!(
                "publishing cached raw {} → {}",
                tmp_path.display(),
                raw_path.display(),
            )
        })?;

    Ok(raw_path)
}

/// Clone a cached raw into a per-instance disk path using `cp -c`,
/// which on macOS uses `clonefile(2)` internally: zero bytes copied,
/// APFS extents shared copy-on-write until writes diverge.
///
/// `dest` must not exist — `cp -c` refuses to overwrite. Callers
/// (`LocalAvfBackend::provision_disk`) guarantee this by only
/// invoking when the per-instance disk is absent.
pub async fn clone_to(src: &Path, dest: &Path) -> anyhow::Result<()> {
    let output = tokio::process::Command::new("cp")
        .arg("-c")
        .arg(src)
        .arg(dest)
        .output()
        .await
        .context("spawning `cp -c`")?;
    if !output.status.success() {
        bail!(
            "`cp -c {} {}` failed (exit {}): {}",
            src.display(),
            dest.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_raw_path_appends_raw_suffix() {
        let qcow2 = Path::new("/cache/images/debian-12.qcow2");
        assert_eq!(
            cached_raw_path_for(qcow2),
            PathBuf::from("/cache/images/debian-12.qcow2.raw"),
        );
    }

    #[test]
    fn cached_raw_path_handles_no_extension() {
        // Defensive — if anyone ever caches a file without `.qcow2`,
        // we still append `.raw` rather than replacing.
        let qcow2 = Path::new("/cache/images/weird-base");
        assert_eq!(
            cached_raw_path_for(qcow2),
            PathBuf::from("/cache/images/weird-base.raw"),
        );
    }

    fn qemu_img_available() -> bool {
        std::process::Command::new("qemu-img")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// End-to-end: build a tiny qcow2 with `qemu-img`, populate the
    /// raw cache via `ensure_cached_raw`, then clone it to two
    /// destinations and assert all three files (cache + both clones)
    /// have byte-identical contents over the qcow2's virtual size.
    ///
    /// This is the integrity guarantee the cache + clonefile path
    /// has to maintain — otherwise an AVF VM created from a warm
    /// cache would silently boot from different bytes than one
    /// created cold.
    #[tokio::test]
    async fn cache_then_clone_produces_byte_identical_disks() {
        if !qemu_img_available() {
            eprintln!("qemu-img not installed — skipping cache_then_clone_produces_byte_identical_disks");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let qcow2 = dir.path().join("source.qcow2");

        // 4 MiB — small enough to compare in memory, large enough
        // to exercise the converter's chunking even though it's
        // sub-CHUNK (8 MiB).
        let status = std::process::Command::new("qemu-img")
            .args([
                "create",
                "-f",
                "qcow2",
                qcow2.to_str().unwrap(),
                "4M",
            ])
            .output()
            .unwrap();
        assert!(status.status.success(), "qemu-img create failed");

        // Cold cache: ensure_cached_raw converts.
        let cached = ensure_cached_raw(&qcow2).await.unwrap();
        assert!(cached.exists(), "cached raw should land on disk");
        assert_eq!(
            cached,
            cached_raw_path_for(&qcow2),
            "cache path should follow the documented `.raw` suffix scheme"
        );

        // Warm cache: ensure_cached_raw is a no-op — returns the
        // same path, doesn't re-convert. We can't easily assert
        // "didn't re-convert" without inspecting mtime, but we can
        // assert the path is stable and the contents unchanged.
        let cold_bytes = std::fs::read(&cached).unwrap();
        let cached_again = ensure_cached_raw(&qcow2).await.unwrap();
        assert_eq!(cached, cached_again);
        let warm_bytes = std::fs::read(&cached).unwrap();
        assert_eq!(cold_bytes, warm_bytes, "cache contents must be stable across calls");

        // Clone — two destinations from the same cache.
        let inst_a = dir.path().join("a.raw");
        let inst_b = dir.path().join("b.raw");
        clone_to(&cached, &inst_a).await.unwrap();
        clone_to(&cached, &inst_b).await.unwrap();
        let a_bytes = std::fs::read(&inst_a).unwrap();
        let b_bytes = std::fs::read(&inst_b).unwrap();
        assert_eq!(
            a_bytes, cold_bytes,
            "clone A must be byte-identical to the cached raw"
        );
        assert_eq!(
            b_bytes, cold_bytes,
            "clone B must be byte-identical to the cached raw"
        );
    }

    /// Modifying a clone must not affect the cached raw —
    /// confirms COW independence at the file-handle level (the
    /// underlying APFS extents may still be shared until divergence,
    /// but `read()` after a `write()` to one side must reflect only
    /// the local change).
    #[tokio::test]
    async fn clone_writes_do_not_leak_to_cached_source() {
        if !qemu_img_available() {
            eprintln!("qemu-img not installed — skipping clone_writes_do_not_leak_to_cached_source");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let qcow2 = dir.path().join("source.qcow2");
        std::process::Command::new("qemu-img")
            .args(["create", "-f", "qcow2", qcow2.to_str().unwrap(), "4M"])
            .output()
            .unwrap();

        let cached = ensure_cached_raw(&qcow2).await.unwrap();
        let cached_before = std::fs::read(&cached).unwrap();

        let inst = dir.path().join("inst.raw");
        clone_to(&cached, &inst).await.unwrap();

        // Stomp a sentinel into the clone.
        let mut tweaked = std::fs::read(&inst).unwrap();
        tweaked[0] = 0xDE;
        tweaked[1] = 0xAD;
        tweaked[2] = 0xBE;
        tweaked[3] = 0xEF;
        std::fs::write(&inst, &tweaked).unwrap();

        let cached_after = std::fs::read(&cached).unwrap();
        assert_eq!(
            cached_before, cached_after,
            "writes to the clone must NOT propagate to the cached source"
        );
        // And the clone really did change (sanity).
        let inst_after = std::fs::read(&inst).unwrap();
        assert_eq!(&inst_after[..4], &[0xDE, 0xAD, 0xBE, 0xEF]);
    }
}
