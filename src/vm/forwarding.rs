//! Runtime port forwarding on a running VM.
//!
//! Each active forward is backed by an agv-spawned supervisor process (see
//! `crate::forward_daemon`) that runs a respawn loop around `ssh -N -L`.
//! This module is the high-level orchestration: spawn the supervisor on
//! add, kill it on stop, surface the live set for `--list`. Supervisor
//! PIDs and origins are mirrored to `<instance>/forwards.toml` so other
//! commands can reason about them.
//!
//! Forwards survive transient SSH failures (the supervisor reconnects) but
//! die with the VM — `forwarding::stop_all_for_vm` is called from stop and
//! destroy so no orphan SSH processes are left to retry against a gone VM.

use std::collections::HashSet;
use std::os::unix::process::CommandExt as _;
use std::process::Stdio;

use anyhow::{bail, Context as _};

use crate::forward::{self, ActiveForward, ForwardSpec, Origin};
use crate::vm::instance::{Instance, Status};

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

/// Spawn a forward supervisor for one spec and return its PID.
///
/// The supervisor is detached: stdio is redirected to /dev/null, it runs
/// in its own process group so we can group-kill it later, and the parent
/// does not wait on it (the OS reaps the zombie when agv exits).
fn spawn_supervisor(vm: &str, spec: ForwardSpec) -> anyhow::Result<u32> {
    let exe = std::env::current_exe().context("failed to locate agv binary")?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("__forward-daemon")
        .arg(vm)
        .arg(spec.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Put the supervisor in its own process group so we can later send a
    // signal to the whole group (kills any in-flight ssh child too).
    cmd.process_group(0);

    let child = cmd
        .spawn()
        .context("failed to spawn forward supervisor")?;
    let pid = child.id();
    // Don't wait on the child — let it run detached. The std::process::Child
    // would reap on drop in newer Rust versions, but explicitly forgetting it
    // makes the intent clear: we hand the process off to the OS.
    std::mem::forget(child);
    Ok(pid)
}

/// Check whether a process with this PID is still alive.
fn is_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Drop entries whose supervisor is no longer running, persisting the
/// trimmed set. Returns the live entries.
async fn sweep_dead(inst: &Instance) -> anyhow::Result<Vec<ActiveForward>> {
    let active = forward::read_active(&inst.forwards_path()).await?;
    let (live, dead): (Vec<_>, Vec<_>) =
        active.into_iter().partition(|a| is_alive(a.pid));
    if !dead.is_empty() {
        forward::write_active(&inst.forwards_path(), &live).await?;
    }
    Ok(live)
}

/// Result of applying config forwards on start/resume.
pub struct ApplyOutcome {
    pub applied: Vec<ActiveForward>,
    /// Specs that failed to spawn a supervisor for. Start does not abort
    /// on these — the VM itself is fine; only the forwards are degraded.
    pub failures: Vec<(ForwardSpec, String)>,
}

/// Apply the list of config forwards to a freshly started VM.
///
/// Called from start/resume. Any previous runtime state is irrelevant on
/// boot, so we tear down stale supervisors and write `forwards.toml`
/// from scratch.
pub async fn apply_config_forwards(
    inst: &Instance,
    specs: &[ForwardSpec],
) -> anyhow::Result<ApplyOutcome> {
    // Kill any leftover supervisors from a previous boot before claiming
    // this fresh slate.
    stop_all_for_instance(inst).await;

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
        match spawn_supervisor(&inst.name, *spec) {
            Ok(pid) => applied.push(ActiveForward::new(*spec, Origin::Config, pid)),
            Err(e) => failures.push((*spec, format!("{e:#}"))),
        }
    }
    forward::write_active(&inst.forwards_path(), &applied).await?;
    Ok(ApplyOutcome { applied, failures })
}

/// Add one or more ad-hoc forwards to a running VM.
///
/// Duplicates of existing (host, proto) pairs are rejected before any
/// supervisor is spawned, so a partial failure can't leave half the list
/// applied.
pub async fn add(name: &str, specs: &[ForwardSpec]) -> anyhow::Result<Vec<ActiveForward>> {
    if specs.is_empty() {
        bail!("no ports specified — run `agv forward <name> --list` to see active forwards");
    }
    forward::validate_unique(specs)?;

    let inst = open_running(name).await?;
    let mut active = sweep_dead(&inst).await?;
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
        let pid = spawn_supervisor(name, *spec)?;
        let entry = ActiveForward::new(*spec, Origin::Adhoc, pid);
        active.push(entry);
        added.push(entry);
        // Persist after each successful add so a mid-list spawn failure
        // still leaves a consistent state file.
        forward::write_active(&inst.forwards_path(), &active).await?;
    }

    Ok(added)
}

/// Read the active forwards on a running VM, sweeping dead supervisors first.
pub async fn list(name: &str) -> anyhow::Result<Vec<ActiveForward>> {
    let inst = open_running(name).await?;
    sweep_dead(&inst).await
}

/// Stop specific forwards by matching on `(host, proto)`.
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
                forward::kill_supervisor(entry.pid);
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
    for entry in &active {
        forward::kill_supervisor(entry.pid);
    }
    forward::clear_active(&inst.forwards_path()).await?;
    Ok(active)
}

/// Best-effort: tear down every supervisor known for a given instance and
/// clear the state file. Used by stop/destroy/reconcile so orphan SSH
/// processes don't keep retrying against a gone VM.
///
/// Errors are swallowed because this runs from cleanup paths where the VM
/// is already gone or going.
pub async fn stop_all_for_instance(inst: &Instance) {
    forward::kill_all_and_clear(&inst.forwards_path()).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_instance(dir: &std::path::Path) -> Instance {
        Instance {
            name: "test-fwd".to_string(),
            dir: dir.to_path_buf(),
        }
    }

    #[tokio::test]
    async fn sweep_dead_removes_stale_entries_and_persists() {
        let dir = tempdir().unwrap();
        let inst = test_instance(dir.path());

        // Pick a definitely-dead PID by spawning and reaping.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = child.id();
        child.wait().unwrap();

        // Use our own PID as a stand-in for an alive supervisor.
        let alive_pid = std::process::id();

        let entries = vec![
            ActiveForward::new(
                ForwardSpec::new(8080, 8080, forward::Proto::Tcp),
                forward::Origin::Adhoc,
                dead_pid,
            ),
            ActiveForward::new(
                ForwardSpec::new(9090, 9090, forward::Proto::Tcp),
                forward::Origin::Config,
                alive_pid,
            ),
        ];
        forward::write_active(&inst.forwards_path(), &entries)
            .await
            .unwrap();

        let live = sweep_dead(&inst).await.unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].pid, alive_pid);

        let on_disk = forward::read_active(&inst.forwards_path()).await.unwrap();
        assert_eq!(on_disk, live);
    }

    #[tokio::test]
    async fn sweep_dead_no_changes_when_all_alive() {
        let dir = tempdir().unwrap();
        let inst = test_instance(dir.path());
        let alive_pid = std::process::id();
        let entries = vec![ActiveForward::new(
            ForwardSpec::new(8080, 8080, forward::Proto::Tcp),
            forward::Origin::Adhoc,
            alive_pid,
        )];
        forward::write_active(&inst.forwards_path(), &entries)
            .await
            .unwrap();
        let live = sweep_dead(&inst).await.unwrap();
        assert_eq!(live, entries);
    }

    #[tokio::test]
    async fn sweep_dead_handles_missing_state_file() {
        let dir = tempdir().unwrap();
        let inst = test_instance(dir.path());
        let live = sweep_dead(&inst).await.unwrap();
        assert!(live.is_empty());
    }
}
