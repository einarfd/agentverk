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
            "broken" => Ok(Self::Broken),
            other => anyhow::bail!("unknown VM status: {other}"),
        }
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
    pub async fn open(name: &str) -> anyhow::Result<Self> {
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

    /// Path to the provisioned marker file.
    #[must_use]
    pub fn provisioned_path(&self) -> PathBuf {
        self.dir.join("provisioned")
    }

    /// Check whether this instance has been provisioned.
    #[must_use]
    pub fn is_provisioned(&self) -> bool {
        self.provisioned_path().exists()
    }

    /// Mark this instance as provisioned by writing a marker file.
    pub async fn mark_provisioned(&self) -> anyhow::Result<()> {
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
            return Ok(Status::Stopped);
        }
        Ok(status)
    }

    /// Check whether the QEMU process (from the PID file) is still alive.
    async fn is_process_alive(&self) -> bool {
        let Ok(raw) = tokio::fs::read_to_string(self.pid_path()).await else {
            return false;
        };
        let Ok(pid) = raw.trim().parse::<u32>() else {
            return false;
        };
        // Use `kill -0` via the shell to check process existence without
        // needing unsafe libc calls.
        tokio::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .is_ok_and(|s| s.success())
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
        assert_eq!(Status::Broken.to_string(), "broken");
    }

    #[test]
    fn status_from_str_all_variants() {
        assert_eq!("creating".parse::<Status>().unwrap(), Status::Creating);
        assert_eq!("configuring".parse::<Status>().unwrap(), Status::Configuring);
        assert_eq!("running".parse::<Status>().unwrap(), Status::Running);
        assert_eq!("stopped".parse::<Status>().unwrap(), Status::Stopped);
        assert_eq!("broken".parse::<Status>().unwrap(), Status::Broken);
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
        // u32::MAX is never a valid PID.
        tokio::fs::write(inst.pid_path(), u32::MAX.to_string())
            .await
            .unwrap();
        tokio::fs::write(inst.qmp_socket_path(), "").await.unwrap();

        assert_eq!(inst.reconcile_status().await.unwrap(), Status::Stopped);
        assert_eq!(inst.read_status().await.unwrap(), Status::Stopped);
        assert!(!inst.pid_path().exists(), "stale pid file should be removed");
        assert!(
            !inst.qmp_socket_path().exists(),
            "stale qmp socket should be removed"
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
