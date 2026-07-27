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

use std::collections::BTreeMap;

use crate::config::AutoForward;
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

/// Open a VM for a forward change, with its reconciled status.
///
/// Looser than [`open_running`]: a stopped or suspended VM is fine, because
/// a forward change lands in the saved config and takes effect on the next
/// start. Only the two transient states are refused — editing config out
/// from under a create or a hardware change would race the writer.
async fn open_editable(name: &str) -> anyhow::Result<(Instance, Status)> {
    let inst = Instance::open(name)?;
    let status = inst.reconcile_status().await?;
    if matches!(status, Status::Creating | Status::Configuring) {
        bail!(
            "VM '{name}' is busy (status: {status}) — \
             wait for it to settle before changing forwards"
        );
    }
    Ok((inst, status))
}

/// Refuse `--temporary` when there are no live forwards to act on.
///
/// A temporary change is by definition "this boot only", so on a VM that
/// isn't running it would do precisely nothing. Failing beats a silent no-op.
fn require_live_for_temporary(name: &str, status: Status, live: bool) -> anyhow::Result<()> {
    if !live {
        bail!(
            "--temporary needs a running VM (status: {status}) — \
             it changes only this boot's forwards, so there is nothing to change. \
             Drop --temporary to edit the saved config for '{name}'."
        );
    }
    Ok(())
}

/// Spawn a forward supervisor for one spec and return its PID.
///
/// The supervisor is detached: stdio is redirected to /dev/null, it runs
/// in its own process group so we can group-kill it later, and the parent
/// does not wait on it (the OS reaps the zombie when agv exits).
fn spawn_supervisor(vm: &str, spec: &ForwardSpec) -> anyhow::Result<u32> {
    let exe = std::env::current_exe().context("failed to locate agv binary")?;
    let mut cmd = std::process::Command::new(&exe);
    // Wire the port mapping as the short `host[:guest]` string, and each bind
    // address as its own `--bind` flag — the string form can't encode binds.
    cmd.arg("__forward-daemon")
        .arg(vm)
        .arg(spec.to_short_string());
    for bind in &spec.binds {
        cmd.arg("--bind").arg(bind.to_string());
    }
    cmd.stdin(Stdio::null())
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

// ---------------------------------------------------------------------------
// The saved (persistent) forwards list in `<instance>/config.toml`
// ---------------------------------------------------------------------------
//
// `agv forward` writes here by default, so a forward added once survives
// every later start/resume. These helpers deliberately do **not** touch
// `<instance>/status`: `vm::config_set` brackets its save with
// `Configuring`/`Stopped` because it may be resizing a disk, but changing a
// forward is safe while the VM runs and must leave its status alone.

/// Read the instance's saved forwards.
pub fn saved_forwards(inst: &Instance) -> anyhow::Result<Vec<ForwardSpec>> {
    Ok(crate::config::load_resolved(&inst.config_path())?.forwards)
}

/// [`saved_forwards`] by VM name, for callers that only have one — the
/// confirmation prompt needs to know what a bare `--rm` would discard
/// before committing to it.
pub fn saved_config_forwards(name: &str) -> anyhow::Result<Vec<ForwardSpec>> {
    saved_forwards(&Instance::open(name)?)
}

/// Append `specs` to the instance's saved forwards.
async fn persist_add(inst: &Instance, specs: &[ForwardSpec]) -> anyhow::Result<()> {
    let mut config = crate::config::load_resolved(&inst.config_path())?;
    config.forwards.extend(specs.iter().cloned());
    crate::config::save(&config, &inst.config_path()).await
}

/// Remove every saved forward whose host port is in `hosts`, returning what
/// was removed.
///
/// Keyed on host port rather than the full spec so removing a port takes all
/// of its bind addresses with it — the same rule [`rm`] applies to live
/// supervisors, so the two halves can't drift apart.
async fn persist_remove(inst: &Instance, hosts: &[u16]) -> anyhow::Result<Vec<ForwardSpec>> {
    let mut config = crate::config::load_resolved(&inst.config_path())?;
    let (removed, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut config.forwards)
        .into_iter()
        .partition(|f| hosts.contains(&f.host));
    if removed.is_empty() {
        return Ok(removed);
    }
    config.forwards = kept;
    crate::config::save(&config, &inst.config_path()).await?;
    Ok(removed)
}

/// Clear the instance's saved forwards, returning what was removed.
async fn persist_clear(inst: &Instance) -> anyhow::Result<Vec<ForwardSpec>> {
    let mut config = crate::config::load_resolved(&inst.config_path())?;
    let removed = std::mem::take(&mut config.forwards);
    if removed.is_empty() {
        return Ok(removed);
    }
    crate::config::save(&config, &inst.config_path()).await?;
    Ok(removed)
}

/// Reject `specs` that would collide with an already-claimed
/// `(bind-address, host-port)`. `what` names the set for the error message.
fn reject_conflicts(
    vm: &str,
    specs: &[ForwardSpec],
    claimed: &HashSet<(String, u16)>,
    what: &str,
) -> anyhow::Result<()> {
    for spec in specs {
        for key in forward::bind_keys(spec) {
            if claimed.contains(&key) {
                bail!(
                    "host port {} on bind {} is already {what} — \
                     remove it first with `agv forward {vm} --rm {}`",
                    spec.host,
                    key.0,
                    spec.host,
                );
            }
        }
    }
    Ok(())
}

/// Drop entries whose supervisor is no longer running, persisting the
/// trimmed set. Returns the live entries.
async fn sweep_dead(inst: &Instance) -> anyhow::Result<Vec<ActiveForward>> {
    let active = forward::read_active(&inst.forwards_path()).await?;
    let (live, dead): (Vec<_>, Vec<_>) =
        active.into_iter().partition(|a| forward::is_alive(a.pid));
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
        match spawn_supervisor(&inst.name, spec) {
            Ok(pid) => applied.push(ActiveForward::new(spec.clone(), Origin::Config, pid)),
            Err(e) => failures.push((spec.clone(), format!("{e:#}"))),
        }
    }
    forward::write_active(&inst.forwards_path(), &applied).await?;
    Ok(ApplyOutcome { applied, failures })
}

/// What [`add`] did.
pub struct AddOutcome {
    /// Forwards now live. Empty when the VM isn't running.
    pub applied: Vec<ActiveForward>,
    /// Specs written to the saved config. Empty for a temporary add.
    pub persisted: Vec<ForwardSpec>,
    /// Whether the VM was running, i.e. whether `applied` means anything.
    pub live: bool,
}

/// Add one or more forwards to a VM.
///
/// Persistent by default: the spec is written to the VM's saved config so it
/// comes back on every later start, *and* applied immediately if the VM is
/// running. `temporary` skips the config write, leaving a forward that lives
/// only until the VM next stops.
///
/// Works on a stopped or suspended VM (config-only), so keeping a forward no
/// longer means stopping the VM to edit its config.
pub async fn add(
    name: &str,
    specs: &[ForwardSpec],
    temporary: bool,
) -> anyhow::Result<AddOutcome> {
    if specs.is_empty() {
        bail!("no ports specified — run `agv forward {name} --list` to see active forwards");
    }
    forward::validate_unique(specs)?;

    let (inst, status) = open_editable(name).await?;
    let live = status == Status::Running;
    if temporary {
        require_live_for_temporary(name, status, live)?;
    }

    // A persistent add must not duplicate what the config already declares,
    // whether or not that entry happens to be live right now.
    if !temporary {
        let claimed: HashSet<(String, u16)> = saved_forwards(&inst)?
            .iter()
            .flat_map(forward::bind_keys)
            .collect();
        reject_conflicts(name, specs, &claimed, "declared in the saved config")?;
    }

    // A live clash blocks the spawn regardless of persistence — two ssh
    // processes can't bind the same (address, port).
    let mut active = if live { sweep_dead(&inst).await? } else { Vec::new() };
    if live {
        let claimed: HashSet<(String, u16)> = active
            .iter()
            .flat_map(|a| forward::bind_keys(&a.spec()))
            .collect();
        reject_conflicts(name, specs, &claimed, "already forwarded on this VM")?;
    }

    // Persist before spawning. The saved config is the durable record; a
    // spawn that fails afterwards is visible immediately and recoverable
    // with `--reapply`, whereas a tunnel with no config entry would silently
    // disappear at the next stop.
    if !temporary {
        persist_add(&inst, specs).await?;
    }

    let mut applied: Vec<ActiveForward> = Vec::new();
    if live {
        let origin = if temporary { Origin::Adhoc } else { Origin::Config };
        for spec in specs {
            let pid = spawn_supervisor(name, spec)?;
            let entry = ActiveForward::new(spec.clone(), origin, pid);
            active.push(entry.clone());
            applied.push(entry);
            // Persist after each successful add so a mid-list spawn failure
            // still leaves a consistent state file.
            forward::write_active(&inst.forwards_path(), &active).await?;
        }
    }

    Ok(AddOutcome {
        applied,
        persisted: if temporary { Vec::new() } else { specs.to_vec() },
        live,
    })
}

/// Read the active forwards on a running VM, sweeping dead supervisors first.
pub async fn list(name: &str) -> anyhow::Result<Vec<ActiveForward>> {
    let inst = open_running(name).await?;
    sweep_dead(&inst).await
}

/// What [`rm`] did.
pub struct RmOutcome {
    /// Live supervisors that were killed.
    pub killed: Vec<ActiveForward>,
    /// Specs dropped from the saved config. Empty for a temporary removal.
    pub unpersisted: Vec<ForwardSpec>,
    /// Whether the VM was running.
    pub live: bool,
}

/// Remove forwards from a VM. An empty `specs` removes every one.
///
/// The mirror image of [`add`]: persistent by default, so removing a forward
/// drops it from the saved config *and* kills its tunnel. `temporary` kills
/// the tunnel only, leaving the config entry to come back on the next start.
///
/// Matching is by host port, so a port with several bind addresses goes as a
/// unit — removing `8080` must not leave a `0.0.0.0`-exposed sibling behind.
pub async fn rm(
    name: &str,
    specs: &[ForwardSpec],
    temporary: bool,
) -> anyhow::Result<RmOutcome> {
    let (inst, status) = open_editable(name).await?;
    let live = status == Status::Running;
    if temporary {
        require_live_for_temporary(name, status, live)?;
    }
    let all = specs.is_empty();

    let mut active = if live {
        forward::read_active(&inst.forwards_path()).await?
    } else {
        Vec::new()
    };

    // Resolve every named port against both the live set and the saved
    // config *before* touching anything, so one typo can't leave the rest
    // half-removed.
    if !all {
        let saved = saved_forwards(&inst)?;
        let unknown: Vec<String> = specs
            .iter()
            .filter(|spec| {
                let in_live = active.iter().any(|a| a.host == spec.host);
                let in_config = !temporary && saved.iter().any(|f| f.host == spec.host);
                !in_live && !in_config
            })
            .map(|spec| spec.host.to_string())
            .collect();
        if !unknown.is_empty() {
            bail!(
                "no forward for: {} on '{name}' — \
                 run `agv forward {name} --list` to see what is active",
                unknown.join(", ")
            );
        }
    }

    let mut killed: Vec<ActiveForward> = Vec::new();
    if live {
        if all {
            for entry in &active {
                forward::kill_supervisor(entry.pid);
            }
            killed = std::mem::take(&mut active);
            forward::clear_active(&inst.forwards_path()).await?;
        } else {
            for spec in specs {
                let matches: Vec<ActiveForward> =
                    active.iter().filter(|a| a.host == spec.host).cloned().collect();
                active.retain(|a| a.host != spec.host);
                for entry in matches {
                    forward::kill_supervisor(entry.pid);
                    killed.push(entry);
                }
            }
            forward::write_active(&inst.forwards_path(), &active).await?;
        }
    }

    let unpersisted = if temporary {
        Vec::new()
    } else if all {
        persist_clear(&inst).await?
    } else {
        let hosts: Vec<u16> = specs.iter().map(|s| s.host).collect();
        persist_remove(&inst, &hosts).await?
    };

    Ok(RmOutcome {
        killed,
        unpersisted,
        live,
    })
}

/// Best-effort: tear down every supervisor known for a given instance and
/// clear the state file. Used by stop/destroy/reconcile so orphan SSH
/// processes don't keep retrying against a gone VM.
///
/// Errors are swallowed because this runs from cleanup paths where the VM
/// is already gone or going.
pub async fn stop_all_for_instance(inst: &Instance) {
    forward::kill_all_and_clear(&inst.forwards_path()).await;
    // Also remove the per-auto-forward port files so they don't mislead
    // consumers after the VM is stopped. Swallow errors — the files may
    // not exist, and the cleanup is best-effort.
    let _ = remove_auto_forward_port_files(inst).await;
}

async fn remove_auto_forward_port_files(inst: &Instance) -> anyhow::Result<()> {
    let mut entries = tokio::fs::read_dir(&inst.dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        if let Some(name) = entry.file_name().to_str() {
            if name.ends_with("_port") && name != "ssh_port" {
                let _ = tokio::fs::remove_file(entry.path()).await;
            }
        }
    }
    Ok(())
}

/// Result of applying auto-allocated forwards on start/resume.
pub struct AutoForwardOutcome {
    pub applied: Vec<(String, ActiveForward)>,
    pub failures: Vec<(String, String)>,
}

/// Apply the resolved config's `auto_forwards` to a freshly started VM.
///
/// For each declared name, allocate a free host port, spawn a supervisor,
/// append to the active-forwards state file (so it's cleaned up on stop
/// alongside every other forward), and write the host port to
/// `<instance>/<name>_port`.
///
/// Call this *after* `apply_config_forwards` so the state file has already
/// been reset for this boot. Failures are non-fatal per-entry — the VM is
/// up, so surface the specific entry that couldn't allocate.
pub async fn apply_auto_forwards(
    inst: &Instance,
    auto_forwards: &BTreeMap<String, AutoForward>,
) -> anyhow::Result<AutoForwardOutcome> {
    let mut outcome = AutoForwardOutcome {
        applied: Vec::with_capacity(auto_forwards.len()),
        failures: Vec::new(),
    };
    if auto_forwards.is_empty() {
        return Ok(outcome);
    }

    let mut active = forward::read_active(&inst.forwards_path()).await?;

    for (name, af) in auto_forwards {
        let host_port = match super::qemu::allocate_free_port().await {
            Ok(p) => p,
            Err(e) => {
                outcome
                    .failures
                    .push((name.clone(), format!("port allocation failed: {e:#}")));
                continue;
            }
        };

        let spec = ForwardSpec::new(host_port, af.guest_port);
        let pid = match spawn_supervisor(&inst.name, &spec) {
            Ok(pid) => pid,
            Err(e) => {
                outcome
                    .failures
                    .push((name.clone(), format!("supervisor spawn failed: {e:#}")));
                continue;
            }
        };

        let entry = ActiveForward::new(spec, Origin::Auto, pid);
        active.push(entry.clone());
        forward::write_active(&inst.forwards_path(), &active).await?;

        // Publish the allocated host port for consumers (`agv gui`, scripts).
        tokio::fs::write(inst.auto_forward_port_path(name), host_port.to_string())
            .await
            .with_context(|| {
                format!(
                    "failed to write auto-forward port file {}",
                    inst.auto_forward_port_path(name).display()
                )
            })?;

        outcome.applied.push((name.clone(), entry));
    }

    Ok(outcome)
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

    /// An instance whose `config.toml` declares `forwards`, for exercising
    /// the persistence helpers without a real VM.
    async fn instance_with_saved_forwards(
        dir: &std::path::Path,
        forwards: Vec<ForwardSpec>,
    ) -> Instance {
        let inst = test_instance(dir);
        let config = crate::config::ResolvedConfig {
            base_url: String::new(),
            base_checksum: String::new(),
            skip_checksum: false,
            memory: "2G".to_string(),
            cpus: 2,
            disk: "20G".to_string(),
            user: "agent".to_string(),
            os_family: "debian".to_string(),
            files: vec![],
            setup: vec![],
            provision: vec![],
            forwards,
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
            backend: "qemu".to_string(),
        };
        crate::config::save(&config, &inst.config_path()).await.unwrap();
        inst
    }

    #[tokio::test]
    async fn persist_add_appends_and_preserves_binds() {
        let dir = tempdir().unwrap();
        let inst = instance_with_saved_forwards(dir.path(), vec![ForwardSpec::new(8080, 8080)]).await;

        let added: ForwardSpec = "5432@0.0.0.0@::1".parse().unwrap();
        persist_add(&inst, std::slice::from_ref(&added)).await.unwrap();

        let saved = saved_forwards(&inst).unwrap();
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[1], added, "binds must survive the save/load round-trip");
    }

    #[tokio::test]
    async fn persist_remove_takes_every_bind_on_the_port() {
        let dir = tempdir().unwrap();
        let inst = instance_with_saved_forwards(
            dir.path(),
            vec![
                "8080@0.0.0.0".parse().unwrap(),
                "8080@10.0.0.1".parse().unwrap(),
                ForwardSpec::new(5432, 5432),
            ],
        )
        .await;

        let removed = persist_remove(&inst, &[8080]).await.unwrap();
        assert_eq!(removed.len(), 2, "both binds on 8080 should go");

        let saved = saved_forwards(&inst).unwrap();
        assert_eq!(saved, vec![ForwardSpec::new(5432, 5432)]);
    }

    #[tokio::test]
    async fn persist_remove_of_absent_port_is_a_no_op() {
        let dir = tempdir().unwrap();
        let inst = instance_with_saved_forwards(dir.path(), vec![ForwardSpec::new(8080, 8080)]).await;

        let removed = persist_remove(&inst, &[9999]).await.unwrap();
        assert!(removed.is_empty());
        assert_eq!(saved_forwards(&inst).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn persist_clear_empties_the_list() {
        let dir = tempdir().unwrap();
        let inst = instance_with_saved_forwards(
            dir.path(),
            vec![ForwardSpec::new(8080, 8080), ForwardSpec::new(5432, 5432)],
        )
        .await;

        let removed = persist_clear(&inst).await.unwrap();
        assert_eq!(removed.len(), 2);
        assert!(saved_forwards(&inst).unwrap().is_empty());
    }

    #[tokio::test]
    async fn persist_helpers_leave_other_config_fields_alone() {
        let dir = tempdir().unwrap();
        let inst = instance_with_saved_forwards(dir.path(), vec![]).await;

        persist_add(&inst, &[ForwardSpec::new(8080, 8080)]).await.unwrap();

        let config = crate::config::load_resolved(&inst.config_path()).unwrap();
        assert_eq!(config.memory, "2G");
        assert_eq!(config.cpus, 2);
        assert_eq!(config.backend, "qemu");
    }

    #[test]
    fn reject_conflicts_flags_a_claimed_bind_and_port() {
        let claimed: HashSet<(String, u16)> =
            forward::bind_keys(&ForwardSpec::new(8080, 8080)).into_iter().collect();

        // Same port on the default loopback bind collides...
        let err = reject_conflicts("vm", &[ForwardSpec::new(8080, 3000)], &claimed, "in use")
            .unwrap_err();
        assert!(format!("{err:#}").contains("already in use"), "got: {err:#}");

        // ...but the same port on a different address does not.
        let elsewhere: ForwardSpec = "8080@10.0.0.1".parse().unwrap();
        reject_conflicts("vm", &[elsewhere], &claimed, "in use").unwrap();
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
                ForwardSpec::new(8080, 8080),
                forward::Origin::Adhoc,
                dead_pid,
            ),
            ActiveForward::new(
                ForwardSpec::new(9090, 9090),
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
            ForwardSpec::new(8080, 8080),
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
