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
        // Look up the SSH endpoint fresh each iteration: if the VM is
        // restarted while we're running, the host/port may change (QEMU
        // hostfwd port reallocates on every boot; AVF NAT IP can also
        // change across restarts).
        let Ok((host, port)) = (match crate::vm::backend::for_instance(&instance) {
            Ok(b) => b.ssh_endpoint(&instance).await,
            Err(e) => Err(e),
        }) else {
            tokio::select! {
                () = tokio::time::sleep(RESPAWN_DELAY) => continue,
                _ = term.recv() => return Ok(()),
                _ = intr.recv() => return Ok(()),
            }
        };

        let mut cmd = build_ssh_command(&instance, &host, port, &user, &spec);
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
    host: &str,
    port: u16,
    user: &str,
    spec: &ForwardSpec,
) -> tokio::process::Command {
    let base = ssh::base_ssh_args(&instance.ssh_key_path(), port);
    let args = supervisor_ssh_args(base, spec, user, host);

    let mut cmd = tokio::process::Command::new("ssh");
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

/// Assemble the full `ssh` argument vector for a forward supervisor.
///
/// Pure (takes the already-built `base` connection args and returns a
/// `Vec<String>`) so the forward-specific wiring — the `ExitOnForwardFailure`
/// toggle, `GatewayPorts`, and one `-L` per bind — is unit-testable without
/// spawning a process.
fn supervisor_ssh_args(
    base: Vec<String>,
    spec: &ForwardSpec,
    user: &str,
    host: &str,
) -> Vec<String> {
    let mut args = base;
    args.push("-N".to_string());
    args.push("-o".to_string());
    // A single forward (loopback default, or one explicit bind) fails fast so
    // the supervisor respawns on a bind error (host port in use, address not
    // up yet). Only with *multiple* binds do we tolerate partial failure —
    // there one address being down shouldn't tear down the ones that do bind.
    // A dropped connection still exits ssh (ServerAlive), so respawn on real
    // outages is unaffected either way.
    if spec.binds.len() > 1 {
        args.push("ExitOnForwardFailure=no".to_string());
    } else {
        args.push("ExitOnForwardFailure=yes".to_string());
    }
    args.push("-o".to_string());
    args.push("ServerAliveInterval=15".to_string());
    args.push("-o".to_string());
    args.push("ServerAliveCountMax=2".to_string());
    // Binding past loopback needs GatewayPorts; an explicit bind_address in
    // `-L` overrides the default, but we set it too as belt-and-suspenders.
    if spec.has_non_loopback_bind() {
        args.push("-o".to_string());
        args.push("GatewayPorts=yes".to_string());
    }
    // One `-L` per bind address (or a single loopback forward when none).
    // The middle `localhost` is the guest-side destination resolved by sshd
    // inside the guest (services bound to the guest's 127.0.0.1).
    for forward in spec.ssh_forward_args() {
        args.push("-L".to_string());
        args.push(forward);
    }
    args.push(format!("{user}@{host}"));
    args
}

#[cfg(test)]
mod tests {
    use super::supervisor_ssh_args;
    use crate::forward::ForwardSpec;

    fn args_for(spec: &ForwardSpec) -> Vec<String> {
        // A synthetic base so the assertions focus on the forward wiring.
        supervisor_ssh_args(vec!["-p".to_string(), "22".to_string()], spec, "agent", "127.0.0.1")
    }

    fn has_opt(args: &[String], opt: &str) -> bool {
        args.windows(2).any(|w| w[0] == "-o" && w[1] == opt)
    }

    fn l_values(args: &[String]) -> Vec<String> {
        args.windows(2)
            .filter(|w| w[0] == "-L")
            .map(|w| w[1].clone())
            .collect()
    }

    #[test]
    fn loopback_default_fails_fast_no_gateway_single_l() {
        let args = args_for(&ForwardSpec::new(8642, 8642));
        assert!(has_opt(&args, "ExitOnForwardFailure=yes"));
        assert!(!has_opt(&args, "GatewayPorts=yes"));
        assert_eq!(l_values(&args), vec!["8642:localhost:8642"]);
        assert_eq!(args.last().unwrap(), "agent@127.0.0.1");
    }

    #[test]
    fn single_bind_still_fails_fast_but_gateways() {
        // One explicit bind keeps fail-fast (nothing else to protect) but
        // needs GatewayPorts to bind past loopback.
        let args = args_for(&ForwardSpec::with_binds(
            8642,
            8642,
            vec!["0.0.0.0".parse().unwrap()],
        ));
        assert!(has_opt(&args, "ExitOnForwardFailure=yes"));
        assert!(has_opt(&args, "GatewayPorts=yes"));
        assert_eq!(l_values(&args), vec!["0.0.0.0:8642:localhost:8642"]);
    }

    #[test]
    fn multi_bind_tolerates_partial_failure_and_brackets_ipv6() {
        let args = args_for(&ForwardSpec::with_binds(
            8642,
            80,
            vec!["192.168.1.5".parse().unwrap(), "2001:db8::5".parse().unwrap()],
        ));
        assert!(has_opt(&args, "ExitOnForwardFailure=no"));
        assert!(has_opt(&args, "GatewayPorts=yes"));
        assert_eq!(
            l_values(&args),
            vec![
                "192.168.1.5:8642:localhost:80",
                "[2001:db8::5]:8642:localhost:80",
            ]
        );
    }

    #[test]
    fn multi_loopback_binds_do_not_request_gateway() {
        // Two loopback binds: partial-tolerance on, but no GatewayPorts.
        let args = args_for(&ForwardSpec::with_binds(
            8642,
            8642,
            vec!["127.0.0.1".parse().unwrap(), "::1".parse().unwrap()],
        ));
        assert!(has_opt(&args, "ExitOnForwardFailure=no"));
        assert!(!has_opt(&args, "GatewayPorts=yes"));
    }
}
