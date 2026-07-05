//! SSH operations — shelling out to system `ssh`, `scp`, and `ssh-keygen`.
//!
//! For v1, we delegate to the system SSH client rather than implementing
//! the protocol directly. This keeps things simple and leverages the user's
//! existing OpenSSH installation.

use std::path::Path;

use anyhow::{bail, Context as _};
use tracing::{debug, info};

use crate::error::Error;
use crate::vm::instance::Instance;

/// Generate an Ed25519 keypair for SSH access to the VM.
///
/// Returns the public key content (for injection into cloud-init).
pub async fn generate_keypair(instance: &Instance) -> anyhow::Result<String> {
    let key_path = instance.ssh_key_path();
    let key_str = key_path
        .to_str()
        .context("SSH key path is not valid UTF-8")?;

    let comment = format!("agv-{}", instance.name);

    info!(path = key_str, "generating Ed25519 keypair");

    let result = tokio::process::Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-f", key_str, "-C", &comment])
        .output()
        .await;

    match result {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("ssh-keygen failed (exit {}): {stderr}", output.status);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("ssh-keygen not found — run 'agv doctor' to check all dependencies");
        }
        Err(e) => {
            return Err(e).context("failed to run ssh-keygen");
        }
    }

    let pub_key = tokio::fs::read_to_string(instance.ssh_pub_key_path())
        .await
        .context("failed to read generated public key")?;

    info!("keypair generated");
    Ok(pub_key.trim().to_string())
}

/// Open an SSH session to a running VM.
///
/// If `command` is empty, opens an interactive session. Otherwise, runs the
/// given command non-interactively.
pub async fn session(
    instance: &Instance,
    user: &str,
    ssh_opts: &[String],
    command: &[String],
) -> anyhow::Result<()> {
    let (host, port) = crate::vm::backend::for_instance(instance)?
        .ssh_endpoint(instance)
        .await?;
    let key_path = instance.ssh_key_path();
    let args = base_ssh_args(&key_path, port);

    let destination = format!("{user}@{host}");

    let mut cmd = tokio::process::Command::new("ssh");
    cmd.args(&args).args(ssh_opts).arg(&destination);

    if !command.is_empty() {
        cmd.arg("--");
        cmd.args(command);
    }

    match cmd.status().await {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => bail!("SSH session exited with {status}"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("ssh not found — run 'agv doctor' to check all dependencies");
        }
        Err(source) => Err(Error::Ssh {
            name: instance.name.clone(),
            source,
        }
        .into()),
    }
}

/// Spawn `ssh` for a one-shot command and collect its raw `Output`
/// (stdout, stderr, and exit status). Shared by [`run_cmd`] and
/// [`run_cmd_stdout`]; callers decide how to treat the streams.
async fn ssh_output(
    instance: &Instance,
    user: &str,
    command: &[String],
) -> anyhow::Result<std::process::Output> {
    let (host, port) = crate::vm::backend::for_instance(instance)?
        .ssh_endpoint(instance)
        .await?;
    let key_path = instance.ssh_key_path();
    let args = base_ssh_args(&key_path, port);

    let destination = format!("{user}@{host}");

    let mut cmd = tokio::process::Command::new("ssh");
    cmd.args(&args).arg(&destination);

    if !command.is_empty() {
        cmd.arg("--");
        cmd.args(command);
    }

    match cmd.output().await {
        Ok(o) => Ok(o),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("ssh not found — run 'agv doctor' to check all dependencies");
        }
        Err(source) => Err(Error::Ssh {
            name: instance.name.clone(),
            source,
        }
        .into()),
    }
}

/// Run a command over SSH, capturing stdout and stderr.
///
/// Returns the combined output as a string. Fails with context if the
/// command exits non-zero. Use this instead of `session()` when the
/// output should be captured rather than forwarded to the terminal.
///
/// Because it folds stderr into the returned string, this is for
/// display/logging — **not** for parsing a command's output. To parse,
/// use [`run_cmd_stdout`], which keeps stderr out of the result.
pub async fn run_cmd(
    instance: &Instance,
    user: &str,
    command: &[String],
) -> anyhow::Result<String> {
    let output = ssh_output(instance, user, command).await?;

    let combined = {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.is_empty() {
            stdout.into_owned()
        } else if stdout.is_empty() {
            stderr.into_owned()
        } else {
            format!("{stdout}{stderr}")
        }
    };

    if !output.status.success() {
        anyhow::bail!(
            "SSH command exited with {}: {}",
            output.status,
            combined.trim()
        );
    }

    Ok(combined)
}

/// Run a command over SSH and return **only stdout** on success.
///
/// Unlike [`run_cmd`], stderr is never folded into the result — it's used
/// solely for the error message on a non-zero exit. Use this whenever the
/// caller parses the command's output: incidental stderr chatter would
/// otherwise corrupt the parse.
///
/// The motivating case is the idle watcher's `who` probe. When the host's
/// SSH forwards `LC_*` to a guest that can't set that locale, the guest
/// shell prints a `setlocale` warning to stderr; folded into an otherwise
/// empty `who` stdout, it made `who` look non-empty, so the VM was seen as
/// active and never auto-suspended.
pub async fn run_cmd_stdout(
    instance: &Instance,
    user: &str,
    command: &[String],
) -> anyhow::Result<String> {
    let output = ssh_output(instance, user, command).await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("SSH command exited with {}: {}", output.status, stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Copy a file into the VM using scp.
pub async fn copy_to(
    instance: &Instance,
    user: &str,
    local_path: &Path,
    remote_path: &str,
) -> anyhow::Result<()> {
    let (host, port) = crate::vm::backend::for_instance(instance)?
        .ssh_endpoint(instance)
        .await?;
    let key_path = instance.ssh_key_path();

    let local_str = local_path
        .to_str()
        .context("local path is not valid UTF-8")?;

    let destination = format!("{user}@{host}:{remote_path}");

    let mut args = vec![
        "-i".to_string(),
        key_path.display().to_string(),
        "-P".to_string(), // scp uses uppercase -P for port
        port.to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=no".to_string(),
        "-o".to_string(),
        "UserKnownHostsFile=/dev/null".to_string(),
        "-o".to_string(),
        "LogLevel=ERROR".to_string(),
    ];
    args.push(local_str.to_string());
    args.push(destination);

    let output = match tokio::process::Command::new("scp")
        .args(&args)
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("scp not found — run 'agv doctor' to check all dependencies");
        }
        Err(source) => {
            return Err(Error::Scp {
                name: instance.name.clone(),
                source,
            }
            .into());
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("scp failed (exit {}): {stderr}", output.status);
    }

    Ok(())
}

/// Copy files between the host and a running VM.
///
/// Paths prefixed with `:` are treated as VM paths; others are local.
/// Supports recursive copy and shows progress when `verbose` is set.
pub async fn transfer(
    instance: &Instance,
    user: &str,
    source: &str,
    dest: &str,
    recursive: bool,
    verbose: bool,
) -> anyhow::Result<()> {
    let (host, port) = crate::vm::backend::for_instance(instance)?
        .ssh_endpoint(instance)
        .await?;
    let key_path = instance.ssh_key_path();

    // Expand :path → user@host:path
    let scp_source = expand_vm_path(source, user, &host);
    let scp_dest = expand_vm_path(dest, user, &host);

    // Determine direction for display.
    let is_upload = !source.starts_with(':');
    let direction = if is_upload { "→ VM" } else { "← VM" };

    let mut args = vec![
        "-i".to_string(),
        key_path.display().to_string(),
        "-P".to_string(),
        port.to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=no".to_string(),
        "-o".to_string(),
        "UserKnownHostsFile=/dev/null".to_string(),
        "-o".to_string(),
        "LogLevel=ERROR".to_string(),
    ];

    if recursive {
        args.push("-r".to_string());
    }

    args.push(scp_source);
    args.push(scp_dest);

    if verbose {
        eprintln!("  {source} {direction} {dest}");
    }

    let output = match tokio::process::Command::new("scp")
        .args(&args)
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("scp not found — run 'agv doctor' to check all dependencies");
        }
        Err(source) => {
            return Err(Error::Scp {
                name: instance.name.clone(),
                source,
            }
            .into());
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("copy failed (exit {}): {stderr}", output.status);
    }

    if verbose {
        eprintln!("  done");
    }

    Ok(())
}

/// Expand a `:path` prefix into `user@host:path` for scp. `host` comes from
/// the backend's `ssh_endpoint` (`127.0.0.1` for QEMU, the guest's NAT IP
/// for AVF).
fn expand_vm_path(path: &str, user: &str, host: &str) -> String {
    if let Some(remote) = path.strip_prefix(':') {
        format!("{user}@{host}:{remote}")
    } else {
        path.to_string()
    }
}

/// Wait for SSH to become available on a VM, polling until ready.
///
/// Retries up to 180 times with 1-second intervals (180s total).
/// The window covers first-boot cloud-init runs on slow setups —
/// AVF specifically needs more time than QEMU because its retry
/// loop only effectively starts once the guest has DHCP (the
/// runner can't report an `ssh_endpoint` before then). With a
/// 60-second budget, a fresh AVF first boot could time out
/// mid-cloud-init with sshd up but the agent user not yet
/// created, giving a misleading "publickey" rejection.
///
/// The backend's `ssh_endpoint` is re-resolved each iteration so the
/// AVF backend can return "no guest IP yet" early in the boot — its
/// IP arrives via guest DHCP, not when the VM process starts. The
/// QEMU backend's endpoint is stable (a host-side port forward), so
/// the re-resolve is a cheap read of one file per iteration.
#[expect(
    clippy::too_many_lines,
    reason = "Single polling state machine — readability comes from keeping the loop, the re-resolve of guest IP per iteration, and the multi-class error parsing (Connection refused, publickey, host-key changed, generic timeout) co-located. Pulling each branch into a helper would obscure the linear timing logic that ties them together."
)]
pub async fn wait_for_ready(instance: &Instance, user: &str) -> anyhow::Result<()> {
    let backend = crate::vm::backend::for_instance(instance)?;
    let key_path = instance.ssh_key_path();
    let start = std::time::Instant::now();

    info!(vm = %instance.name, "waiting for SSH to become ready");

    // Track the most recent observation so the timeout error can
    // distinguish "guest never got an IP" from "guest had an IP but
    // SSH kept refusing" — two very different problems with two
    // very different fixes.
    let mut last_endpoint: Option<(String, u16)> = None;
    let mut last_endpoint_error: Option<String> = None;
    let mut last_ssh_failure: Option<String> = None;

    for attempt in 1..=180 {
        let endpoint = backend.ssh_endpoint(instance).await;
        let (host, port) = match endpoint {
            Ok(hp) => {
                last_endpoint = Some(hp.clone());
                hp
            }
            Err(e) => {
                last_endpoint_error = Some(format!("{e:#}"));
                debug!(
                    vm = %instance.name,
                    attempt,
                    error = %format!("{e:#}"),
                    "endpoint not ready yet, retrying in 1s"
                );
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };
        let args = base_ssh_args(&key_path, port);
        let destination = format!("{user}@{host}");

        let output = tokio::process::Command::new("ssh")
            .args(&args)
            .arg(&destination)
            .arg("true")
            .output()
            .await;

        match &output {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                bail!("ssh not found — run 'agv doctor' to check all dependencies");
            }
            _ => {}
        }

        match output {
            Ok(o) if o.status.success() => {
                let elapsed = start.elapsed();
                info!(
                    vm = %instance.name,
                    elapsed_secs = elapsed.as_secs(),
                    "SSH ready after {attempt} attempt(s)"
                );
                return Ok(());
            }
            Ok(o) => {
                // Capture stderr for the diagnostic; ssh emits
                // useful clues here ("Connection refused", "Host
                // key verification failed", "Permission denied",
                // etc.).
                let stderr = String::from_utf8_lossy(&o.stderr);
                last_ssh_failure = Some(stderr.trim().to_string());
            }
            Err(e) => {
                last_ssh_failure = Some(format!("spawn error: {e}"));
            }
        }

        debug!(
            vm = %instance.name,
            attempt,
            "SSH not ready yet, retrying in 1s"
        );
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    let diagnostic = match (last_endpoint, last_ssh_failure, last_endpoint_error) {
        (Some((host, port)), Some(err), _) => {
            let publickey_hint = if err.contains("Permission denied (publickey)") {
                "\n  This usually means cloud-init hadn't finished creating \
                 the guest user / installing the SSH key by the timeout. Try \
                 `agv start --retry <name>`."
            } else {
                ""
            };
            format!(
                "  Last endpoint:  {host}:{port}\n  Last SSH error: {err}\n  \
                 VM is reachable on the network; sshd may still be coming \
                 up, the SSH key may have drifted, or the guest user may \
                 not be ready yet.{publickey_hint}"
            )
        }
        (Some((host, port)), None, _) => format!(
            "  Last endpoint:  {host}:{port}\n  No SSH error captured — the \
             VM accepted no connection attempts. Check that sshd is running \
             in the guest."
        ),
        (None, _, Some(err)) => format!(
            "  No endpoint ever resolved.\n  Last error:     {err}\n  \
             For AVF: the guest never got a DHCP lease the runner could \
             see. Typical causes are stale cloud-init network state from a \
             pre-migration QEMU boot (try `agv backend migrate-to-avf` \
             again, which regenerates the seed with a fresh instance-id so \
             cloud-init re-runs networking) or sshd never coming up at all."
        ),
        (None, _, None) => {
            "  No endpoint ever resolved; no underlying error captured.".to_string()
        }
    };

    Err(Error::SshTimeout {
        name: instance.name.clone(),
        diagnostic,
    }
    .into())
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Read the SSH port from the instance's `ssh_port` file.
pub(crate) async fn ssh_port(instance: &Instance) -> anyhow::Result<u16> {
    let path = instance.ssh_port_path();
    let raw = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("failed to read SSH port file {}", path.display()))?;

    raw.trim()
        .parse::<u16>()
        .with_context(|| format!("invalid SSH port in {}: {raw:?}", path.display()))
}

/// Build the common SSH arguments used by all operations.
pub(crate) fn base_ssh_args(key_path: &Path, port: u16) -> Vec<String> {
    vec![
        "-i".to_string(),
        key_path.display().to_string(),
        "-p".to_string(),
        port.to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=no".to_string(),
        "-o".to_string(),
        "UserKnownHostsFile=/dev/null".to_string(),
        "-o".to_string(),
        "LogLevel=ERROR".to_string(),
        "-o".to_string(),
        "ConnectTimeout=5".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_ssh_args_contains_expected_flags() {
        let args = base_ssh_args(Path::new("/tmp/id_ed25519"), 2222);

        assert!(args.contains(&"-i".to_string()));
        assert!(args.contains(&"/tmp/id_ed25519".to_string()));
        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"2222".to_string()));
        assert!(args.contains(&"StrictHostKeyChecking=no".to_string()));
        assert!(args.contains(&"UserKnownHostsFile=/dev/null".to_string()));
        assert!(args.contains(&"LogLevel=ERROR".to_string()));
        assert!(args.contains(&"ConnectTimeout=5".to_string()));
    }

    #[tokio::test]
    async fn generate_keypair_creates_key_files() {
        let dir = tempfile::tempdir().unwrap();
        let instance = Instance {
            name: "test-keygen".to_string(),
            dir: dir.path().to_path_buf(),
        };

        let pub_key = generate_keypair(&instance).await.unwrap();

        // Both files should exist.
        assert!(instance.ssh_key_path().exists());
        assert!(instance.ssh_pub_key_path().exists());

        // Public key should be an Ed25519 key.
        assert!(
            pub_key.starts_with("ssh-ed25519 "),
            "expected ssh-ed25519 prefix, got: {pub_key}"
        );

        // Comment should contain the VM name.
        assert!(
            pub_key.contains("agv-test-keygen"),
            "expected agv-test-keygen comment, got: {pub_key}"
        );
    }

    #[tokio::test]
    async fn ssh_port_reads_and_parses() {
        let dir = tempfile::tempdir().unwrap();
        let instance = Instance {
            name: "test-port".to_string(),
            dir: dir.path().to_path_buf(),
        };

        tokio::fs::write(instance.ssh_port_path(), "2222\n")
            .await
            .unwrap();

        let port = ssh_port(&instance).await.unwrap();
        assert_eq!(port, 2222);
    }

    #[tokio::test]
    async fn ssh_port_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let instance = Instance {
            name: "test-noport".to_string(),
            dir: dir.path().to_path_buf(),
        };

        let result = ssh_port(&instance).await;
        assert!(result.is_err());
        let err = format!("{:#}", result.unwrap_err());
        assert!(
            err.contains("failed to read SSH port file"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn expand_vm_path_with_colon_prefix() {
        assert_eq!(
            expand_vm_path(":~/file.txt", "agent", "127.0.0.1"),
            "agent@127.0.0.1:~/file.txt"
        );
    }

    #[test]
    fn expand_vm_path_with_absolute_remote() {
        assert_eq!(
            expand_vm_path(":/tmp/file", "agent", "127.0.0.1"),
            "agent@127.0.0.1:/tmp/file"
        );
    }

    #[test]
    fn expand_vm_path_local_unchanged() {
        assert_eq!(
            expand_vm_path("./local/file.txt", "agent", "127.0.0.1"),
            "./local/file.txt"
        );
    }

    #[test]
    fn expand_vm_path_absolute_local_unchanged() {
        assert_eq!(
            expand_vm_path("/tmp/file.txt", "agent", "127.0.0.1"),
            "/tmp/file.txt"
        );
    }

    #[test]
    fn expand_vm_path_custom_user_and_host() {
        assert_eq!(
            expand_vm_path(":~/data", "myuser", "192.168.64.5"),
            "myuser@192.168.64.5:~/data"
        );
    }
}
