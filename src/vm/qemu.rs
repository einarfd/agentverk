//! QEMU process spawning and QMP protocol communication.
//!
//! Handles starting QEMU as a background process, communicating over the
//! QMP JSON socket for lifecycle management, and graceful/forceful shutdown.

use crate::vm::instance::Instance;

/// Spawn a QEMU process for the given VM instance.
pub async fn start(_instance: &Instance) -> anyhow::Result<()> {
    todo!("spawn QEMU process with overlay disk, seed image, and QMP socket")
}

/// Send a graceful shutdown command via the QMP socket.
pub async fn stop(_instance: &Instance) -> anyhow::Result<()> {
    todo!("send quit command via QMP socket")
}

/// Force-kill the QEMU process using the PID file.
pub async fn force_stop(_instance: &Instance) -> anyhow::Result<()> {
    todo!("kill QEMU process from PID file")
}
