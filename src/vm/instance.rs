//! VM instance state — status, paths, and metadata.
//!
//! Each VM instance has a directory under `<data_dir>/instances/<name>/`
//! containing its disk, SSH keys, config, and status file.

use std::fmt;
use std::path::PathBuf;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

/// The possible states of a VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// `agv create` is currently in progress.
    Creating,
    /// `agv config set` is currently applying hardware changes.
    Configuring,
    /// QEMU process is active.
    Running,
    /// VM exists but is not running.
    Stopped,
    /// VM is suspended — full state saved to disk, can be resumed.
    Suspended,
    /// Creation or provisioning failed partway through.
    Broken,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Creating => write!(f, "creating"),
            Self::Configuring => write!(f, "configuring"),
            Self::Running => write!(f, "running"),
            Self::Stopped => write!(f, "stopped"),
            Self::Suspended => write!(f, "suspended"),
            Self::Broken => write!(f, "broken"),
        }
    }
}

impl std::str::FromStr for Status {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "creating" => Ok(Self::Creating),
            "configuring" => Ok(Self::Configuring),
            "running" => Ok(Self::Running),
            "stopped" => Ok(Self::Stopped),
            "suspended" => Ok(Self::Suspended),
            "broken" => Ok(Self::Broken),
            other => anyhow::bail!("unknown VM status: {other}"),
        }
    }
}

/// First-boot provisioning phase.
///
/// Tracks where the first-boot flow is at, so a failed provisioning can be
/// resumed from the same point via `agv start --retry` instead of restarting
/// from scratch (which would re-run already-completed steps).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Waiting for the SSH server inside the VM to become reachable.
    SshWait,
    /// Copying `[[files]]` entries into the VM via SCP.
    Files,
    /// Running `[[setup]]` steps as root.
    Setup,
    /// Running `[[provision]]` steps as the user.
    Provision,
    /// All first-boot work has completed successfully.
    Complete,
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SshWait => write!(f, "ssh wait"),
            Self::Files => write!(f, "files"),
            Self::Setup => write!(f, "setup"),
            Self::Provision => write!(f, "provision"),
            Self::Complete => write!(f, "complete"),
        }
    }
}

/// Persisted progress through first-boot provisioning.
///
/// Stored at `<instance>/provision_state` as TOML. The `index` field is the
/// position of the *next* step to run within the phase, so it equals the
/// number of completed steps in the current phase. When `phase` is
/// `Complete`, `index` is unused.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvisionState {
    pub phase: Phase,
    /// Next step index to run within the current phase (0-based).
    #[serde(default)]
    pub index: usize,
    /// Total number of steps in the current phase, for display purposes.
    #[serde(default)]
    pub total: usize,
    /// Error message from the last failed step, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ProvisionState {
    /// Build a state representing "fresh — never started provisioning yet".
    #[must_use]
    pub fn fresh() -> Self {
        Self {
            phase: Phase::SshWait,
            index: 0,
            total: 0,
            error: None,
        }
    }

    /// Build a state representing "all done".
    #[must_use]
    pub fn complete() -> Self {
        Self {
            phase: Phase::Complete,
            index: 0,
            total: 0,
            error: None,
        }
    }

    /// Has provisioning finished successfully?
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.phase == Phase::Complete
    }
}

/// In-memory representation of a VM instance.
#[derive(Debug)]
pub struct Instance {
    /// VM name.
    pub name: String,
    /// Root directory for this instance's state.
    pub dir: PathBuf,
}

impl Instance {
    /// Open an existing instance by name.
    pub fn open(name: &str) -> anyhow::Result<Self> {
        let dir = crate::dirs::instance_dir(name)?;
        anyhow::ensure!(
            dir.exists(),
            crate::error::Error::VmNotFound {
                name: name.to_string()
            }
        );
        Ok(Self {
            name: name.to_string(),
            dir,
        })
    }

    /// Path to the status file.
    #[must_use]
    pub fn status_path(&self) -> PathBuf {
        self.dir.join("status")
    }

    /// Path to the qcow2 overlay disk.
    #[must_use]
    pub fn disk_path(&self) -> PathBuf {
        self.dir.join("disk.qcow2")
    }

    /// Path to the cloud-init seed image.
    #[must_use]
    pub fn seed_path(&self) -> PathBuf {
        self.dir.join("seed.iso")
    }

    /// Path to the SSH private key.
    #[must_use]
    pub fn ssh_key_path(&self) -> PathBuf {
        self.dir.join("id_ed25519")
    }

    /// Path to the SSH public key.
    #[must_use]
    pub fn ssh_pub_key_path(&self) -> PathBuf {
        self.dir.join("id_ed25519.pub")
    }

    /// Path to the file storing the localhost port forwarded to guest SSH.
    #[must_use]
    pub fn ssh_port_path(&self) -> PathBuf {
        self.dir.join("ssh_port")
    }

    /// Path to the PID file.
    #[must_use]
    pub fn pid_path(&self) -> PathBuf {
        self.dir.join("pid")
    }

    /// Path to the QMP socket.
    #[must_use]
    pub fn qmp_socket_path(&self) -> PathBuf {
        self.dir.join("qmp.sock")
    }

    /// Path to the per-instance EFI NVRAM vars file (aarch64 only).
    #[must_use]
    pub fn efi_vars_path(&self) -> PathBuf {
        self.dir.join("efi-vars.fd")
    }

    /// Path to the serial console log.
    #[must_use]
    pub fn serial_log_path(&self) -> PathBuf {
        self.dir.join("serial.log")
    }

    /// Path to the error log (present only when status is broken).
    #[must_use]
    pub fn error_log_path(&self) -> PathBuf {
        self.dir.join("error.log")
    }

    /// Path to the stored config copy.
    #[must_use]
    pub fn config_path(&self) -> PathBuf {
        self.dir.join("config.toml")
    }

    /// Path to the provisioning log file.
    #[must_use]
    pub fn provision_log_path(&self) -> PathBuf {
        self.dir.join("provision.log")
    }

    /// Path to the active port-forwards state file.
    #[must_use]
    pub fn forwards_path(&self) -> PathBuf {
        self.dir.join("forwards.toml")
    }

    /// Path to the idle-watcher's PID file.
    ///
    /// Present only while a watcher is running for this VM (spawned at
    /// `start`/`resume` when `idle_suspend_minutes > 0`). The file holds the
    /// supervisor process's decimal PID; cleanup paths kill the PID and
    /// remove the file alongside the forward supervisors.
    #[must_use]
    pub fn idle_watcher_pid_path(&self) -> PathBuf {
        self.dir.join("idle_watcher.pid")
    }

    /// Path to the auto-allocated host port for a named auto-forward.
    ///
    /// Mixins declare `[auto_forwards.<name>]`; at VM start agv picks a free
    /// host port, writes the decimal port number here, and spawns an SSH
    /// tunnel. Consumers (`agv gui`, third-party scripts) can read this
    /// file to discover the port without needing to parse `forwards.toml`.
    #[must_use]
    pub fn auto_forward_port_path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}_port"))
    }

    /// Path to the legacy provisioned marker file.
    ///
    /// Newer VMs use `provision_state` instead. This file is kept for
    /// backward compatibility with VMs created before the state machine
    /// was added.
    #[must_use]
    pub fn provisioned_path(&self) -> PathBuf {
        self.dir.join("provisioned")
    }

    /// Path to the provision state file.
    #[must_use]
    pub fn provision_state_path(&self) -> PathBuf {
        self.dir.join("provision_state")
    }

    /// Read the persisted provision state.
    ///
    /// Returns:
    /// - The parsed `ProvisionState` if `provision_state` exists.
    /// - `Complete` if only the legacy `provisioned` touch file exists.
    /// - `Fresh` if neither exists.
    pub async fn read_provision_state(&self) -> ProvisionState {
        let path = self.provision_state_path();
        if let Ok(contents) = tokio::fs::read_to_string(&path).await {
            if let Ok(state) = toml::from_str::<ProvisionState>(&contents) {
                return state;
            }
        }
        if self.provisioned_path().exists() {
            return ProvisionState::complete();
        }
        ProvisionState::fresh()
    }

    /// Write the provision state to disk.
    pub async fn write_provision_state(&self, state: &ProvisionState) -> anyhow::Result<()> {
        let toml_str = toml::to_string(state)
            .with_context(|| format!("failed to serialize provision state for VM '{}'", self.name))?;
        tokio::fs::write(self.provision_state_path(), toml_str)
            .await
            .with_context(|| format!("failed to write provision state for VM '{}'", self.name))
    }

    /// Check whether this instance has finished provisioning.
    ///
    /// This is a synchronous best-effort check used during start to decide
    /// whether to run first-boot. Falls back to the legacy marker file when
    /// the state file is missing.
    #[must_use]
    pub fn is_provisioned(&self) -> bool {
        let state_path = self.provision_state_path();
        if let Ok(contents) = std::fs::read_to_string(&state_path) {
            if let Ok(state) = toml::from_str::<ProvisionState>(&contents) {
                return state.is_complete();
            }
        }
        self.provisioned_path().exists()
    }

    /// Mark this instance as fully provisioned.
    pub async fn mark_provisioned(&self) -> anyhow::Result<()> {
        self.write_provision_state(&ProvisionState::complete()).await?;
        // Also write the legacy marker so older versions of agv would still
        // recognize the VM as provisioned.
        tokio::fs::write(self.provisioned_path(), "")
            .await
            .with_context(|| format!("failed to write provisioned marker for VM '{}'", self.name))
    }

    /// Read the current status from disk.
    pub async fn read_status(&self) -> anyhow::Result<Status> {
        let raw = tokio::fs::read_to_string(self.status_path())
            .await
            .with_context(|| format!("failed to read status for VM '{}'", self.name))?;
        raw.parse()
    }

    /// Write a new status to disk.
    pub async fn write_status(&self, status: Status) -> anyhow::Result<()> {
        tokio::fs::write(self.status_path(), status.to_string())
            .await
            .with_context(|| format!("failed to write status for VM '{}'", self.name))
    }

    /// Check if the QEMU process is still alive. If a PID file exists but the
    /// process is gone, transition to `stopped` and clean up stale files.
    pub async fn reconcile_status(&self) -> anyhow::Result<Status> {
        let status = self.read_status().await?;
        if status == Status::Running && !self.is_process_alive().await {
            self.write_status(Status::Stopped).await?;
            // Clean up stale runtime files.
            let _ = tokio::fs::remove_file(self.pid_path()).await;
            let _ = tokio::fs::remove_file(self.qmp_socket_path()).await;
            let _ = tokio::fs::remove_file(self.ssh_port_path()).await;
            // Kill any forward supervisors before removing forwards.toml,
            // otherwise they keep retrying against a VM that is gone.
            crate::forward::kill_all_and_clear(&self.forwards_path()).await;
            // Remove the managed SSH config entry so `ssh <name>` doesn't
            // try to connect to a stale port.
            let _ = crate::ssh_config::remove_entry(&self.name).await;
            return Ok(Status::Stopped);
        }
        Ok(status)
    }

    /// Check whether the QEMU process (from the PID file) is still alive.
    pub async fn is_process_alive(&self) -> bool {
        let Ok(raw) = tokio::fs::read_to_string(self.pid_path()).await else {
            return false;
        };
        let Ok(pid) = raw.trim().parse::<u32>() else {
            return false;
        };
        crate::forward::pid_from_u32(pid)
            .is_some_and(|p| rustix::process::test_kill_process(p).is_ok())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn test_instance(dir: &std::path::Path) -> Instance {
        Instance {
            name: "test-vm".to_string(),
            dir: dir.to_path_buf(),
        }
    }

    #[test]
    fn status_display() {
        assert_eq!(Status::Creating.to_string(), "creating");
        assert_eq!(Status::Configuring.to_string(), "configuring");
        assert_eq!(Status::Running.to_string(), "running");
        assert_eq!(Status::Stopped.to_string(), "stopped");
        assert_eq!(Status::Suspended.to_string(), "suspended");
        assert_eq!(Status::Broken.to_string(), "broken");
    }

    #[test]
    fn status_from_str_all_variants() {
        assert_eq!("creating".parse::<Status>().unwrap(), Status::Creating);
        assert_eq!("configuring".parse::<Status>().unwrap(), Status::Configuring);
        assert_eq!("running".parse::<Status>().unwrap(), Status::Running);
        assert_eq!("stopped".parse::<Status>().unwrap(), Status::Stopped);
        assert_eq!("suspended".parse::<Status>().unwrap(), Status::Suspended);
        assert_eq!("broken".parse::<Status>().unwrap(), Status::Broken);
    }

    #[test]
    fn provision_state_fresh_is_ssh_wait() {
        let s = ProvisionState::fresh();
        assert_eq!(s.phase, Phase::SshWait);
        assert_eq!(s.index, 0);
        assert!(!s.is_complete());
    }

    #[test]
    fn provision_state_complete_is_complete() {
        let s = ProvisionState::complete();
        assert_eq!(s.phase, Phase::Complete);
        assert!(s.is_complete());
    }

    #[tokio::test]
    async fn provision_state_roundtrips_to_disk() {
        let dir = tempdir().unwrap();
        let inst = test_instance(dir.path());
        let original = ProvisionState {
            phase: Phase::Provision,
            index: 3,
            total: 5,
            error: Some("step 3: nope".to_string()),
        };
        inst.write_provision_state(&original).await.unwrap();
        let loaded = inst.read_provision_state().await;
        assert_eq!(loaded.phase, Phase::Provision);
        assert_eq!(loaded.index, 3);
        assert_eq!(loaded.total, 5);
        assert_eq!(loaded.error.as_deref(), Some("step 3: nope"));
    }

    #[tokio::test]
    async fn provision_state_missing_returns_fresh() {
        let dir = tempdir().unwrap();
        let inst = test_instance(dir.path());
        let s = inst.read_provision_state().await;
        assert_eq!(s.phase, Phase::SshWait);
        assert_eq!(s.index, 0);
    }

    #[tokio::test]
    async fn provision_state_legacy_marker_returns_complete() {
        let dir = tempdir().unwrap();
        let inst = test_instance(dir.path());
        // Write the legacy touch file but no provision_state.
        tokio::fs::write(inst.provisioned_path(), "").await.unwrap();
        let s = inst.read_provision_state().await;
        assert_eq!(s.phase, Phase::Complete);
        assert!(s.is_complete());
    }

    #[tokio::test]
    async fn is_provisioned_uses_state_file() {
        let dir = tempdir().unwrap();
        let inst = test_instance(dir.path());
        assert!(!inst.is_provisioned());

        inst.write_provision_state(&ProvisionState {
            phase: Phase::Provision,
            index: 2,
            total: 5,
            error: None,
        })
        .await
        .unwrap();
        assert!(!inst.is_provisioned(), "partial state should not count as provisioned");

        inst.write_provision_state(&ProvisionState::complete())
            .await
            .unwrap();
        assert!(inst.is_provisioned());
    }

    #[tokio::test]
    async fn is_provisioned_falls_back_to_legacy_marker() {
        let dir = tempdir().unwrap();
        let inst = test_instance(dir.path());
        // No provision_state file, but legacy marker exists.
        tokio::fs::write(inst.provisioned_path(), "").await.unwrap();
        assert!(inst.is_provisioned());
    }

    #[tokio::test]
    async fn mark_provisioned_writes_both_files() {
        let dir = tempdir().unwrap();
        let inst = test_instance(dir.path());
        inst.mark_provisioned().await.unwrap();
        assert!(inst.provision_state_path().exists());
        assert!(inst.provisioned_path().exists());
        let s = inst.read_provision_state().await;
        assert!(s.is_complete());
    }

    #[test]
    fn status_from_str_trims_whitespace() {
        assert_eq!("  running  ".parse::<Status>().unwrap(), Status::Running);
    }

    #[test]
    fn status_from_str_unknown_fails() {
        let result = "unknown".parse::<Status>();
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("unknown VM status"));
    }

    #[test]
    fn path_getters_return_expected_filenames() {
        let dir = tempdir().unwrap();
        let inst = test_instance(dir.path());
        assert_eq!(inst.status_path(), dir.path().join("status"));
        assert_eq!(inst.disk_path(), dir.path().join("disk.qcow2"));
        assert_eq!(inst.seed_path(), dir.path().join("seed.iso"));
        assert_eq!(inst.ssh_key_path(), dir.path().join("id_ed25519"));
        assert_eq!(inst.ssh_pub_key_path(), dir.path().join("id_ed25519.pub"));
        assert_eq!(inst.ssh_port_path(), dir.path().join("ssh_port"));
        assert_eq!(inst.pid_path(), dir.path().join("pid"));
        assert_eq!(inst.qmp_socket_path(), dir.path().join("qmp.sock"));
        assert_eq!(inst.efi_vars_path(), dir.path().join("efi-vars.fd"));
        assert_eq!(inst.serial_log_path(), dir.path().join("serial.log"));
        assert_eq!(inst.error_log_path(), dir.path().join("error.log"));
        assert_eq!(inst.config_path(), dir.path().join("config.toml"));
        assert_eq!(inst.provision_log_path(), dir.path().join("provision.log"));
        assert_eq!(inst.provisioned_path(), dir.path().join("provisioned"));
    }

    #[tokio::test]
    async fn write_and_read_status_roundtrip() {
        let dir = tempdir().unwrap();
        let inst = test_instance(dir.path());
        for status in [Status::Creating, Status::Configuring, Status::Running, Status::Stopped, Status::Broken] {
            inst.write_status(status).await.unwrap();
            assert_eq!(inst.read_status().await.unwrap(), status);
        }
    }

    #[tokio::test]
    async fn read_status_missing_file_errors() {
        let dir = tempdir().unwrap();
        let inst = test_instance(dir.path());
        assert!(inst.read_status().await.is_err());
    }

    #[tokio::test]
    async fn provisioned_marker() {
        let dir = tempdir().unwrap();
        let inst = test_instance(dir.path());
        assert!(!inst.is_provisioned());
        inst.mark_provisioned().await.unwrap();
        assert!(inst.is_provisioned());
    }

    #[tokio::test]
    async fn reconcile_status_stopped_passthrough() {
        let dir = tempdir().unwrap();
        let inst = test_instance(dir.path());
        inst.write_status(Status::Stopped).await.unwrap();
        assert_eq!(inst.reconcile_status().await.unwrap(), Status::Stopped);
    }

    #[tokio::test]
    async fn reconcile_status_broken_passthrough() {
        let dir = tempdir().unwrap();
        let inst = test_instance(dir.path());
        inst.write_status(Status::Broken).await.unwrap();
        assert_eq!(inst.reconcile_status().await.unwrap(), Status::Broken);
    }

    #[tokio::test]
    async fn reconcile_status_running_no_pid_file_transitions_to_stopped() {
        let dir = tempdir().unwrap();
        let inst = test_instance(dir.path());
        inst.write_status(Status::Running).await.unwrap();
        // No PID file — process considered dead.
        assert_eq!(inst.reconcile_status().await.unwrap(), Status::Stopped);
        assert_eq!(inst.read_status().await.unwrap(), Status::Stopped);
    }

    #[tokio::test]
    async fn reconcile_status_stale_pid_transitions_to_stopped() {
        let dir = tempdir().unwrap();
        let inst = test_instance(dir.path());
        inst.write_status(Status::Running).await.unwrap();
        // Spawn a short-lived process, wait for it to exit, then use its
        // PID — guaranteed to be dead and a valid PID number.
        let child = tokio::process::Command::new("true")
            .spawn()
            .expect("failed to spawn 'true'");
        let dead_pid = child.id().expect("child has no PID");
        let _ = child.wait_with_output().await;
        tokio::fs::write(inst.pid_path(), dead_pid.to_string())
            .await
            .unwrap();
        tokio::fs::write(inst.qmp_socket_path(), "").await.unwrap();
        tokio::fs::write(inst.ssh_port_path(), "2222").await.unwrap();

        assert_eq!(inst.reconcile_status().await.unwrap(), Status::Stopped);
        assert_eq!(inst.read_status().await.unwrap(), Status::Stopped);
        assert!(!inst.pid_path().exists(), "stale pid file should be removed");
        assert!(
            !inst.qmp_socket_path().exists(),
            "stale qmp socket should be removed"
        );
        assert!(
            !inst.ssh_port_path().exists(),
            "stale ssh_port file should be removed"
        );
    }

    #[tokio::test]
    async fn reconcile_status_alive_pid_stays_running() {
        let dir = tempdir().unwrap();
        let inst = test_instance(dir.path());
        inst.write_status(Status::Running).await.unwrap();
        // Write our own PID — current process is definitely alive.
        tokio::fs::write(inst.pid_path(), std::process::id().to_string())
            .await
            .unwrap();

        assert_eq!(inst.reconcile_status().await.unwrap(), Status::Running);
        assert_eq!(inst.read_status().await.unwrap(), Status::Running);
        assert!(inst.pid_path().exists(), "pid file should be untouched");
    }
}
