//! SSH operations — shelling out to system `ssh` and `scp` binaries.
//!
//! For v1, we delegate to the system SSH client rather than implementing
//! the protocol directly. This keeps things simple and leverages the user's
//! existing SSH agent and config.

use std::path::Path;

use crate::vm::instance::Instance;

/// Open an interactive SSH session to a running VM.
pub async fn session(_instance: &Instance, _command: &[String]) -> anyhow::Result<()> {
    todo!("exec ssh -i <key> user@<ip> with optional command")
}

/// Copy a file into the VM using scp.
pub async fn copy_to(
    _instance: &Instance,
    _local_path: &Path,
    _remote_path: &str,
) -> anyhow::Result<()> {
    todo!("exec scp -i <key> local_path user@<ip>:remote_path")
}

/// Wait for SSH to become available on a VM, polling until ready.
pub async fn wait_for_ready(_instance: &Instance) -> anyhow::Result<()> {
    todo!("poll SSH connection until available, timeout after 60s")
}
