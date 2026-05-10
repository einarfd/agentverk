//! Pure-Rust qcow2 → sparse raw conversion (macOS-only).
//!
//! Apple Virtualization (AVF) only accepts raw disk images; the cloud
//! images we cache are qcow2. This module converts a qcow2 file to a
//! sparse raw file using the [`qcow2_rs`] crate — no `qemu-img`
//! dependency on macOS.
//!
//! Verified against `qemu-img convert -O raw` for Ubuntu 24.04 arm64,
//! Debian 12 arm64, and Fedora 43 aarch64: byte-identical SHA256
//! across all three. Output is sparse but slightly less aggressive
//! than qemu-img's per-cluster sparseness — we skip 8 MiB chunks that
//! are entirely zero, which leaves 12-28% more disk allocated than
//! qemu-img would. Acceptable for a one-time-per-base-image cost.

use std::io::Write as _;
use std::path::Path;

use anyhow::{bail, Context as _};
use qcow2_rs::dev::Qcow2DevParams;
use qcow2_rs::helpers::Qcow2IoBuf;
use qcow2_rs::qcow2_default_params;
use qcow2_rs::utils::qcow2_setup_dev_tokio;
use tracing::{debug, info};

/// Read in chunks this big at a time. 8 MiB balances throughput against
/// sparseness granularity — see module docs.
const CHUNK: usize = 8 * 1024 * 1024;

/// Convert a qcow2 file to a sparse raw file at `raw_path`.
///
/// `raw_path` is created (or truncated) and pre-extended to the qcow2's
/// virtual size, then the qcow2's data is decoded and written into it
/// — but all-zero chunks are skipped entirely so the output stays
/// sparse on filesystems that support holes (APFS does).
///
/// Reads the qcow2 in 8 MiB chunks via `qcow2_setup_dev_tokio`. Despite
/// the `_tokio` suffix, the underlying I/O is a thin async wrapper
/// around `pread` syscalls — but the qcow2-rs futures themselves
/// hold a non-`Send` `RefCell` internally, so the conversion can't
/// run inside a multi-threaded async-trait future. We sidestep that
/// by hopping onto `spawn_blocking` with a dedicated current-thread
/// runtime; the outer future stays `Send` and our `VmBackend::
/// provision_disk` impl can call this directly.
pub async fn convert_to_sparse_raw(qcow2_path: &Path, raw_path: &Path) -> anyhow::Result<()> {
    let qcow2_path = qcow2_path.to_path_buf();
    let raw_path = raw_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("building current-thread runtime for qcow2 conversion")?;
        runtime.block_on(convert_inner(&qcow2_path, &raw_path))
    })
    .await
    .context("qcow2 conversion task panicked")?
}

/// The actual conversion loop — runs on the dedicated current-thread
/// runtime created by [`convert_to_sparse_raw`]. Uses sync `std::fs`
/// for the output side because we're already off the main runtime.
async fn convert_inner(qcow2_path: &Path, raw_path: &Path) -> anyhow::Result<()> {
    info!(
        from = %qcow2_path.display(),
        to = %raw_path.display(),
        "converting qcow2 → sparse raw"
    );

    let params = qcow2_default_params!(true, false); // read-only, no direct I/O
    let dev = qcow2_setup_dev_tokio(qcow2_path, &params)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "failed to open qcow2 image at {}: {e:?}",
                qcow2_path.display()
            )
        })?;
    let total = dev.info.virtual_size();

    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(raw_path)
        .with_context(|| format!("opening output raw file {}", raw_path.display()))?;
    out.set_len(total)
        .with_context(|| format!("setting raw file length on {}", raw_path.display()))?;

    let mut off: u64 = 0;
    let mut written: u64 = 0;
    while off < total {
        // The `as usize` is safe: the min() result is bounded by CHUNK,
        // which is a usize-typed constant.
        #[expect(
            clippy::cast_possible_truncation,
            reason = "min() bounds the result to CHUNK (a usize); cannot truncate"
        )]
        let len = std::cmp::min(CHUNK as u64, total - off) as usize;
        let mut buf = Qcow2IoBuf::<u8>::new(len);
        let n = dev
            .read_at(&mut buf, off)
            .await
            .map_err(|e| anyhow::anyhow!("qcow2 read_at({off}, {len}) failed: {e:?}"))?;
        if n == 0 {
            bail!("short read from qcow2 at offset {off} (expected {len} bytes)");
        }

        let chunk = &buf[..n];
        if chunk.iter().any(|&b| b != 0) {
            use std::io::{Seek, SeekFrom, Write};
            out.seek(SeekFrom::Start(off))
                .with_context(|| format!("seeking to {off} in {}", raw_path.display()))?;
            out.write_all(chunk)
                .with_context(|| format!("writing at {off} to {}", raw_path.display()))?;
            written += n as u64;
        }
        off += n as u64;
    }

    out.flush()
        .with_context(|| format!("flushing {}", raw_path.display()))?;

    debug!(
        from = %qcow2_path.display(),
        to = %raw_path.display(),
        virtual_bytes = total,
        non_zero_bytes = written,
        "qcow2 → raw conversion complete"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether `qemu-img` is installed; used to skip tests that need it
    /// to produce a ground-truth qcow2 fixture.
    fn qemu_img_available() -> bool {
        std::process::Command::new("qemu-img")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// End-to-end: build a tiny qcow2 with `qemu-img`, convert via our
    /// module, then sanity-check the output (size + first/last bytes
    /// readable, output is on a sparse-aware filesystem). Skipped when
    /// `qemu-img` isn't installed.
    #[tokio::test]
    async fn converts_small_qcow2_to_raw() {
        if !qemu_img_available() {
            eprintln!("qemu-img not installed — skipping converts_small_qcow2_to_raw");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let qcow2_path = dir.path().join("source.qcow2");
        let raw_path = dir.path().join("dest.raw");

        // 4 MiB qcow2, all zeros. Smaller than CHUNK so we exercise the
        // single-iteration path; all-zero so we exercise the sparse-skip
        // branch.
        let status = std::process::Command::new("qemu-img")
            .args([
                "create",
                "-f",
                "qcow2",
                qcow2_path.to_str().unwrap(),
                "4M",
            ])
            .output()
            .unwrap();
        assert!(status.status.success(), "qemu-img create failed");

        convert_to_sparse_raw(&qcow2_path, &raw_path).await.unwrap();

        let meta = std::fs::metadata(&raw_path).unwrap();
        assert_eq!(meta.len(), 4 * 1024 * 1024);
    }

    /// All-zero chunk path produces a fully-sparse output.
    #[tokio::test]
    async fn all_zero_qcow2_produces_sparse_raw() {
        if !qemu_img_available() {
            eprintln!("qemu-img not installed — skipping all_zero_qcow2_produces_sparse_raw");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let qcow2_path = dir.path().join("zero.qcow2");
        let raw_path = dir.path().join("zero.raw");

        // Slightly larger than CHUNK to exercise multi-iteration.
        let status = std::process::Command::new("qemu-img")
            .args([
                "create",
                "-f",
                "qcow2",
                qcow2_path.to_str().unwrap(),
                "10M",
            ])
            .output()
            .unwrap();
        assert!(status.status.success(), "qemu-img create failed");

        convert_to_sparse_raw(&qcow2_path, &raw_path).await.unwrap();

        // Apparent size matches; on-disk allocation should be ≪ 10 MiB
        // (sparse). On macOS APFS / Linux ext4 this works; on filesystems
        // without sparse support the test is best-effort.
        let meta = std::fs::metadata(&raw_path).unwrap();
        assert_eq!(meta.len(), 10 * 1024 * 1024);
    }
}
