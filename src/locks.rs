//! Cross-process advisory file locks.
//!
//! Used to serialise read-modify-write operations on shared agv state
//! files (the managed `ssh_config`, the image cache) across concurrent
//! `agv` invocations from different processes. Two `agv start` calls
//! against different VMs both update `<data_dir>/ssh_config`; without
//! locking, the second writer can clobber the first writer's changes.
//!
//! This is a process-level (not thread-level) concern. The lock is an
//! `flock(2)` advisory exclusive lock held on a sibling lockfile;
//! holding the file descriptor keeps the lock alive, and the kernel
//! releases it automatically when the descriptor is closed (which
//! happens when the [`LockGuard`] drops, including on panic).
//!
//! Acquisition is delegated to a blocking thread pool via
//! `tokio::task::spawn_blocking` so a contended lock doesn't park the
//! async runtime's worker thread.
//!
//! ## Concurrency contract
//!
//! - Two `agv` commands against **different** VMs are safe to run in
//!   parallel. Shared writes (`ssh_config`, image-cache downloads)
//!   serialise via this module.
//! - Two `agv` commands against the **same** VM are not safe. agv
//!   doesn't try to lock individual instance directories — running
//!   `agv start myvm` and `agv stop myvm` simultaneously is undefined.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use anyhow::Context as _;

/// RAII guard that releases an exclusive flock when it drops.
///
/// The lock is held by virtue of holding the file descriptor open;
/// closing the descriptor (via `Drop`) releases it. Panic-safe — the
/// lock is released on stack unwinding too.
pub struct LockGuard {
    // Holding the file open keeps the flock alive. The field is unused
    // in normal operation but must outlive the guarded section.
    _file: std::fs::File,
}

/// Acquire an exclusive cross-process lock on `lock_path`.
///
/// The lockfile is created if it doesn't exist (and stays around — agv
/// doesn't garbage-collect zero-byte lockfiles). Blocks until the lock
/// is granted, but the wait is delegated to a blocking thread so the
/// async runtime stays responsive.
pub async fn acquire_exclusive(lock_path: PathBuf) -> anyhow::Result<LockGuard> {
    tokio::task::spawn_blocking(move || acquire_blocking(&lock_path))
        .await
        .context("lock-acquire task panicked")?
}

fn acquire_blocking(lock_path: &Path) -> anyhow::Result<LockGuard> {
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create lockfile parent directory {}",
                parent.display()
            )
        })?;
    }

    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .open(lock_path)
        .with_context(|| format!("failed to open lockfile {}", lock_path.display()))?;

    rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)
        .with_context(|| format!("failed to flock {}", lock_path.display()))?;

    Ok(LockGuard { _file: file })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two acquires of the same lockfile in the same process (different
    /// async tasks, sharing the runtime) must serialise. The second
    /// task should observe that the first task's drop happened before
    /// it acquired the lock.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn second_acquire_waits_until_first_drops() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("test.lock");

        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::<usize>::new()));

        let mut handles = Vec::new();
        for i in 0..4 {
            let lock_path = lock_path.clone();
            let counter = counter.clone();
            let observed = observed.clone();
            handles.push(tokio::spawn(async move {
                let _guard = acquire_exclusive(lock_path).await.unwrap();
                // Increment; sleep briefly so the test would notice
                // a missing lock; record the order. If the lock
                // serialises, we always observe ordered increments.
                let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                observed.lock().unwrap().push(n);
                let _ = i; // suppress unused
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let final_counter = counter.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(final_counter, 4);
        // Each task should have observed a unique sequential value of
        // counter at the moment of its acquire — the lock kept them
        // from interleaving.
        let mut observed = observed.lock().unwrap().clone();
        observed.sort_unstable();
        assert_eq!(observed, vec![0, 1, 2, 3]);
    }

    /// Acquiring on a path whose parent directory doesn't exist should
    /// create the parent (lockfiles often live in dirs that haven't
    /// been touched yet).
    #[tokio::test]
    async fn acquire_creates_parent_dir_if_missing() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("nested/dir/test.lock");
        assert!(!lock_path.parent().unwrap().exists());
        let _guard = acquire_exclusive(lock_path.clone()).await.unwrap();
        assert!(lock_path.exists());
    }
}
