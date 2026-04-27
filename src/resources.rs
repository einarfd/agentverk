//! Host-vs-VM resource accounting.
//!
//! Two questions this module answers:
//!
//!   1. What is the host actually capable of?  (`probe_host`)
//!   2. What has agv already promised to other VMs? (`probe_allocated`)
//!
//! Combined into a `ResourceReport` for `agv resources` and used by the
//! pre-flight check on `agv create --start` so an agent doesn't oversubscribe
//! the host by spinning up a 16G VM on an 8G machine.

use std::path::Path;

use anyhow::Context as _;
use serde::Serialize;

use crate::vm::instance::{Instance, Status};

/// What the host machine has.
#[derive(Debug, Clone, Serialize)]
pub struct HostResources {
    /// Total physical RAM, in bytes.
    pub total_memory_bytes: u64,
    /// RAM the kernel reports as in-use, in bytes. Reported instead of
    /// "free" because sysinfo's `available_memory` is unreliable on macOS
    /// (returns 0); `used_memory` is consistent across both platforms.
    /// Subtract from `total_memory_bytes` for a reasonable "free" estimate
    /// when the kernel-aware value isn't available.
    pub used_memory_bytes: u64,
    /// Number of logical CPUs the host exposes.
    pub cpus: u32,
    /// Free disk space on the partition holding agv's data dir, in bytes.
    pub data_dir_free_bytes: u64,
}

/// What agv VMs are currently using on the host.
#[derive(Debug, Clone, Default, Serialize)]
pub struct AllocatedResources {
    /// RAM committed to running VMs (Configuring/Running/Creating).
    pub running_memory_bytes: u64,
    /// vCPUs committed to running VMs.
    pub running_cpus: u32,
    /// Number of running VMs.
    pub running_count: u32,
    /// RAM that *would* be committed if every VM were running.
    pub total_memory_bytes: u64,
    /// vCPUs across all VMs.
    pub total_cpus: u32,
    /// Sum of declared disk sizes across all VMs (qcow2 max sizes — actual
    /// usage is typically much lower because of copy-on-write).
    pub total_disk_bytes: u64,
    /// Total VMs known to agv (running + stopped + suspended + broken).
    pub total_count: u32,
}

/// Combined snapshot — host capacity plus agv's footprint on it.
#[derive(Debug, Clone, Serialize)]
pub struct ResourceReport {
    pub host: HostResources,
    pub allocated: AllocatedResources,
}

/// Probe the host for RAM, CPU count, and free disk in `data_dir`'s partition.
pub fn probe_host(data_dir: &Path) -> anyhow::Result<HostResources> {
    use sysinfo::System;

    let mut sys = System::new();
    sys.refresh_memory();
    let total_memory_bytes = sys.total_memory();
    let used_memory_bytes = sys.used_memory();

    let cpus_usize = std::thread::available_parallelism().map_or(0, usize::from);
    let cpus = u32::try_from(cpus_usize).unwrap_or(u32::MAX);

    let data_dir_free_bytes = data_dir_free(data_dir)?;

    Ok(HostResources {
        total_memory_bytes,
        used_memory_bytes,
        cpus,
        data_dir_free_bytes,
    })
}

/// Look up the disk that hosts `dir` and return its available space in bytes.
fn data_dir_free(dir: &Path) -> anyhow::Result<u64> {
    use sysinfo::Disks;

    let disks = Disks::new_with_refreshed_list();
    // Pick the disk whose mount point is the longest prefix of `dir`. This
    // copes with overlay setups (e.g. /home is a separate mount on Linux)
    // by preferring the most specific match.
    let canonical = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());

    let best = disks
        .iter()
        .filter(|d| canonical.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .with_context(|| {
            format!(
                "could not find disk mount point covering {}",
                canonical.display()
            )
        })?;

    Ok(best.available_space())
}

/// Walk the supplied list of instances, read each saved config, and sum
/// memory / cpus / disk. Best-effort: instances whose config doesn't load
/// or doesn't parse are skipped silently (they wouldn't be bootable
/// anyway, so they don't represent live host pressure).
pub async fn probe_allocated(instances: &[Instance]) -> anyhow::Result<AllocatedResources> {
    let mut report = AllocatedResources::default();

    for inst in instances {
        let cfg_path = inst.config_path();
        if !cfg_path.exists() {
            continue;
        }
        let Ok(cfg) = crate::config::load_resolved(&cfg_path) else {
            continue;
        };

        let memory_bytes = crate::image::parse_disk_size(&cfg.memory).unwrap_or(0);
        let disk_bytes = crate::image::parse_disk_size(&cfg.disk).unwrap_or(0);
        let cpus = cfg.cpus;

        let status = inst.read_status().await.unwrap_or(Status::Stopped);
        let is_running = matches!(
            status,
            Status::Running | Status::Configuring | Status::Creating
        );

        if is_running {
            report.running_memory_bytes = report.running_memory_bytes.saturating_add(memory_bytes);
            report.running_cpus = report.running_cpus.saturating_add(cpus);
            report.running_count = report.running_count.saturating_add(1);
        }
        report.total_memory_bytes = report.total_memory_bytes.saturating_add(memory_bytes);
        report.total_cpus = report.total_cpus.saturating_add(cpus);
        report.total_disk_bytes = report.total_disk_bytes.saturating_add(disk_bytes);
        report.total_count = report.total_count.saturating_add(1);
    }

    Ok(report)
}

/// Combined report — host probe + allocation walk over every instance.
pub async fn report() -> anyhow::Result<ResourceReport> {
    let data_dir = crate::dirs::data_dir()?;
    let host = probe_host(&data_dir)?;
    let instances = crate::vm::list().await?;
    let allocated = probe_allocated(&instances).await?;
    Ok(ResourceReport { host, allocated })
}

/// Threshold at which a new VM's memory would be considered to oversubscribe
/// the host: 90% of total physical RAM. Tuned conservatively — the host's
/// own working set (browser, IDE, agv itself) needs the remaining slack.
const MEMORY_OVERCOMMIT_THRESHOLD: f64 = 0.9;

/// Pre-flight check used before booting a new VM. Returns `Ok(())` when the
/// boot is safe; returns a user-facing error when it would push allocated
/// RAM above the threshold and `force` is false.
///
/// The error message tells the agent (or human) exactly which VM is the
/// problem and how to recover (stop a running VM, or pass `--force`).
pub fn check_capacity(
    new_memory_bytes: u64,
    host: &HostResources,
    allocated: &AllocatedResources,
    force: bool,
) -> anyhow::Result<()> {
    if force {
        return Ok(());
    }
    if host.total_memory_bytes == 0 {
        // We couldn't probe the host (e.g. sysinfo returned 0). Don't block
        // the user on a missing measurement; let QEMU's own error surface
        // if RAM is actually short.
        return Ok(());
    }

    let projected = allocated
        .running_memory_bytes
        .saturating_add(new_memory_bytes);

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        reason = "threshold math: 64-bit byte counts well under f64 precision; sign always positive"
    )]
    let threshold = (host.total_memory_bytes as f64 * MEMORY_OVERCOMMIT_THRESHOLD) as u64;

    if projected > threshold {
        let g = |b: u64| {
            #[expect(
                clippy::cast_precision_loss,
                reason = "display formatting; loss at byte scale doesn't matter"
            )]
            let v = b as f64 / (1024.0 * 1024.0 * 1024.0);
            v
        };
        let message = format!(
            "starting this VM would push committed host RAM to {projected:.1} GiB \
             (host has {total:.1} GiB total). Running VMs already use {running:.1} GiB; \
             this VM wants {new:.1} GiB. Stop or destroy a running VM first \
             (`agv ls`, `agv stop <name>`), or pass `--force` to override.",
            projected = g(projected),
            total = g(host.total_memory_bytes),
            running = g(allocated.running_memory_bytes),
            new = g(new_memory_bytes),
        );
        return Err(crate::error::Error::HostCapacity { message }.into());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(total: u64, used: u64) -> HostResources {
        HostResources {
            total_memory_bytes: total,
            used_memory_bytes: used,
            cpus: 8,
            data_dir_free_bytes: 100 * 1024 * 1024 * 1024,
        }
    }

    fn allocated(running: u64) -> AllocatedResources {
        AllocatedResources {
            running_memory_bytes: running,
            ..Default::default()
        }
    }

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn check_capacity_passes_when_under_threshold() {
        let host = host(16 * GIB, 8 * GIB);
        let alloc = allocated(2 * GIB);
        // 2G running + 4G new = 6G, well under 90% of 16G.
        check_capacity(4 * GIB, &host, &alloc, false).unwrap();
    }

    #[test]
    fn check_capacity_refuses_when_over_threshold() {
        let host = host(16 * GIB, 4 * GIB);
        let alloc = allocated(8 * GIB);
        // 8G running + 8G new = 16G, way over 90% of 16G.
        let err = check_capacity(8 * GIB, &host, &alloc, false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("--force"), "error should mention --force: {msg}");
        assert!(
            msg.contains("agv stop") || msg.contains("agv ls"),
            "error should suggest cleanup actions: {msg}",
        );
    }

    #[test]
    fn check_capacity_force_bypasses_threshold() {
        let host = host(16 * GIB, GIB);
        let alloc = allocated(15 * GIB);
        check_capacity(8 * GIB, &host, &alloc, true).unwrap();
    }

    #[test]
    fn check_capacity_skips_when_host_total_unknown() {
        // sysinfo returned 0 — don't block on a missing measurement.
        let host = host(0, 0);
        let alloc = allocated(0);
        check_capacity(8 * GIB, &host, &alloc, false).unwrap();
    }

    #[test]
    fn check_capacity_at_exact_threshold_passes() {
        // 14.4G is exactly 90% of 16G; allow it.
        let host = host(16 * GIB, 8 * GIB);
        let alloc = allocated(0);
        let exactly_at_threshold = (16 * GIB * 9) / 10;
        check_capacity(exactly_at_threshold, &host, &alloc, false).unwrap();
    }

    #[test]
    fn host_probe_returns_nonzero_values() {
        // Smoke test against the real host. We can't predict the values
        // but they should all be > 0 on any reasonable test machine.
        let tmp = tempfile::tempdir().unwrap();
        let host = probe_host(tmp.path()).unwrap();
        assert!(host.total_memory_bytes > 0, "total memory should be > 0");
        assert!(host.cpus > 0, "cpu count should be > 0");
        // free memory and free disk *can* be 0 on weird systems; only
        // assert what we know is universal.
    }
}
