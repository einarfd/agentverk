//! Port-forward supervisor: keeps a single `ssh -N -L` child running by
//! respawning it on exit.
//!
//! Spawned as a detached process by `agv forward` (and by start/resume for
//! config-declared forwards). The parent stores the supervisor's PID in
//! `<instance>/forwards.toml`; stopping the forward means killing that PID
//! (or its process group). The supervisor itself keeps restarting the
//! inner `ssh` as long as it is not signalled.

use std::process::Stdio;
use std::time::Duration;

use anyhow::Context as _;
use tokio::signal::unix::{signal, SignalKind};

use crate::forward::ForwardSpec;
use crate::ssh;
use crate::vm::instance::Instance;

/// Backoff between `ssh` respawn attempts.
///
/// Short enough that a transient failure (VM reboot, sshd blip) is barely
/// noticed; long enough that we don't spin when the VM is truly unreachable.
const RESPAWN_DELAY: Duration = Duration::from_secs(2);

/// Run the supervisor loop for a single forward until killed by a signal.
///
/// Blocks forever under normal operation. Returns `Ok(())` only after
/// receiving SIGTERM or SIGINT, at which point the current `ssh` child has
/// been asked to exit.
pub async fn run(vm: &str, spec: ForwardSpec) -> anyhow::Result<()> {
    let instance = Instance::open(vm)?;
    let user = read_user(&instance)?;

    let mut term = signal(SignalKind::terminate())
        .context("failed to install SIGTERM handler")?;
    let mut intr =
        signal(SignalKind::interrupt()).context("failed to install SIGINT handler")?;

    loop {
        // Look up the SSH port fresh each iteration: if the VM is restarted
        // while we're running, the forwarded port may change.
        let Ok(port) = ssh::ssh_port(&instance).await else {
            tokio::select! {
                () = tokio::time::sleep(RESPAWN_DELAY) => continue,
                _ = term.recv() => return Ok(()),
                _ = intr.recv() => return Ok(()),
            }
        };

        let mut cmd = build_ssh_command(&instance, port, &user, spec);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("agv forward supervisor: ssh spawn failed: {e:#}");
                tokio::select! {
                    () = tokio::time::sleep(RESPAWN_DELAY) => continue,
                    _ = term.recv() => return Ok(()),
                    _ = intr.recv() => return Ok(()),
                }
            }
        };

        tokio::select! {
            status = child.wait() => {
                // ssh exited on its own — log nothing at stable status, just
                // wait a beat and respawn. If signalled, the select below
                // would have fired instead.
                let _ = status;
                tokio::select! {
                    () = tokio::time::sleep(RESPAWN_DELAY) => {}
                    _ = term.recv() => return Ok(()),
                    _ = intr.recv() => return Ok(()),
                }
            }
            _ = term.recv() => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Ok(());
            }
            _ = intr.recv() => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Ok(());
            }
        }
    }
}

fn read_user(instance: &Instance) -> anyhow::Result<String> {
    let config = crate::config::load_resolved(&instance.config_path())?;
    Ok(config.user)
}

fn build_ssh_command(
    instance: &Instance,
    port: u16,
    user: &str,
    spec: ForwardSpec,
) -> tokio::process::Command {
    let mut args = ssh::base_ssh_args(&instance.ssh_key_path(), port);
    args.push("-N".to_string());
    args.push("-o".to_string());
    args.push("ExitOnForwardFailure=yes".to_string());
    args.push("-o".to_string());
    args.push("ServerAliveInterval=15".to_string());
    args.push("-o".to_string());
    args.push("ServerAliveCountMax=2".to_string());
    args.push("-L".to_string());
    args.push(format!("{h}:localhost:{g}", h = spec.host, g = spec.guest));
    args.push(format!("{user}@localhost"));

    let mut cmd = tokio::process::Command::new("ssh");
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}
