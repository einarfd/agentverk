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
