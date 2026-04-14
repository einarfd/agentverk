//! Runtime port forwarding on a running VM.
//!
//! Forwards are applied on a running VM via QEMU's hostfwd mechanism (see
//! [`crate::vm::qemu::hostfwd_add`]). This module handles the high-level
//! add/list/stop operations driven by `agv forward` and mirrors them into
//! the per-instance `forwards.toml` so their origin can be reported later.

use std::collections::HashSet;

use anyhow::bail;

use crate::forward::{self, ActiveForward, ForwardSpec, Origin};
use crate::vm::instance::{Instance, Status};
use crate::vm::qemu;

/// Ensure the VM is running and return an opened [`Instance`].
async fn open_running(name: &str) -> anyhow::Result<Instance> {
    let inst = Instance::open(name)?;
    let status = inst.reconcile_status().await?;
    if status != Status::Running {
        bail!(
            "VM '{name}' is not running (status: {status}). \
             Start it with: agv start {name}"
        );
    }
    Ok(inst)
}

/// Result of applying config forwards on start/resume.
pub struct ApplyOutcome {
    pub applied: Vec<ActiveForward>,
    /// Specs that failed to apply, with the underlying error message. Start
    /// continues even if some forwards fail (e.g. host port already in use)
    /// — the VM itself is fine, only the forwards are degraded.
    pub failures: Vec<(ForwardSpec, String)>,
}

/// Apply the list of config forwards to a freshly started VM.
///
/// Called from start/resume after the QMP socket is up. The VM has just
/// booted, so any previous runtime state is gone — we write
/// `forwards.toml` from scratch.
///
/// Failures to apply individual forwards are collected and returned; start
/// does not abort on them. The caller decides how to surface them.
pub async fn apply_config_forwards(
    inst: &Instance,
    specs: &[ForwardSpec],
) -> anyhow::Result<ApplyOutcome> {
    if specs.is_empty() {
        forward::clear_active(&inst.forwards_path()).await?;
        return Ok(ApplyOutcome {
            applied: Vec::new(),
            failures: Vec::new(),
        });
    }

    let mut applied: Vec<ActiveForward> = Vec::with_capacity(specs.len());
    let mut failures: Vec<(ForwardSpec, String)> = Vec::new();
    for spec in specs {
        match qemu::hostfwd_add(inst, *spec).await {
            Ok(()) => applied.push(ActiveForward::new(*spec, Origin::Config)),
            Err(e) => failures.push((*spec, format!("{e:#}"))),
        }
    }
    forward::write_active(&inst.forwards_path(), &applied).await?;
    Ok(ApplyOutcome { applied, failures })
}

/// Add one or more ad-hoc forwards to a running VM.
///
/// Duplicates of existing (host, proto) pairs are rejected before any QMP
/// calls are made, so a partial failure can't leave half the list applied.
pub async fn add(name: &str, specs: &[ForwardSpec]) -> anyhow::Result<Vec<ActiveForward>> {
    if specs.is_empty() {
        bail!("no ports specified — run `agv forward <name> --list` to see active forwards");
    }
    forward::validate_unique(specs)?;

    let inst = open_running(name).await?;
    let mut active = forward::read_active(&inst.forwards_path()).await?;
    let existing: HashSet<(u16, forward::Proto)> =
        active.iter().map(|a| (a.host, a.proto)).collect();

    for spec in specs {
        if existing.contains(&(spec.host, spec.proto)) {
            bail!(
                "forward for host port {}/{} is already active — use `agv forward {name} --stop {}` first",
                spec.host,
                spec.proto,
                spec,
            );
        }
    }

    let mut added: Vec<ActiveForward> = Vec::with_capacity(specs.len());
    for spec in specs {
        qemu::hostfwd_add(&inst, *spec).await?;
        let entry = ActiveForward::new(*spec, Origin::Adhoc);
        active.push(entry);
        added.push(entry);
        // Persist after each successful add so a mid-list QMP failure still
        // leaves a consistent state file.
        forward::write_active(&inst.forwards_path(), &active).await?;
    }

    Ok(added)
}

/// Read the active forwards on a running VM.
pub async fn list(name: &str) -> anyhow::Result<Vec<ActiveForward>> {
    let inst = open_running(name).await?;
    forward::read_active(&inst.forwards_path()).await
}

/// Stop specific forwards by matching on `(host, proto)`.
///
/// Returns the removed entries. Specs that don't match anything produce an
/// error listing the unknown ports, so typos are surfaced instead of silently
/// succeeding.
pub async fn stop(name: &str, specs: &[ForwardSpec]) -> anyhow::Result<Vec<ActiveForward>> {
    let inst = open_running(name).await?;
    let mut active = forward::read_active(&inst.forwards_path()).await?;

    let mut unknown: Vec<String> = Vec::new();
    let mut removed: Vec<ActiveForward> = Vec::new();

    for spec in specs {
        match active
            .iter()
            .position(|a| a.host == spec.host && a.proto == spec.proto)
        {
            Some(idx) => {
                let entry = active.remove(idx);
                qemu::hostfwd_remove(&inst, entry.spec()).await?;
                removed.push(entry);
                forward::write_active(&inst.forwards_path(), &active).await?;
            }
            None => unknown.push(format!("{}/{}", spec.host, spec.proto)),
        }
    }

    if !unknown.is_empty() {
        bail!(
            "no active forward for: {} — run `agv forward {name} --list` to see active forwards",
            unknown.join(", ")
        );
    }

    Ok(removed)
}

/// Stop every active forward on the VM.
pub async fn stop_all(name: &str) -> anyhow::Result<Vec<ActiveForward>> {
    let inst = open_running(name).await?;
    let active = forward::read_active(&inst.forwards_path()).await?;

    let mut removed: Vec<ActiveForward> = Vec::with_capacity(active.len());
    let mut remaining = active.clone();
    for entry in &active {
        // Find and drop this entry from the remaining list before the QMP
        // call so write_active never describes state we don't hold.
        if let Some(idx) = remaining
            .iter()
            .position(|a| a.host == entry.host && a.proto == entry.proto)
        {
            remaining.remove(idx);
        }
        qemu::hostfwd_remove(&inst, entry.spec()).await?;
        forward::write_active(&inst.forwards_path(), &remaining).await?;
        removed.push(*entry);
    }

    Ok(removed)
}
