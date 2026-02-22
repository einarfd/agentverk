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
            bail!("ssh-keygen not found — is OpenSSH installed?");
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
    command: &[String],
) -> anyhow::Result<()> {
    let port = ssh_port(instance).await?;
    let key_path = instance.ssh_key_path();
    let args = base_ssh_args(&key_path, port);

    let destination = format!("{user}@localhost");

    let mut cmd = tokio::process::Command::new("ssh");
    cmd.args(&args).arg(&destination);

    if !command.is_empty() {
        cmd.arg("--");
        cmd.args(command);
    }

    let status = cmd.status().await.map_err(|source| Error::Ssh {
        name: instance.name.clone(),
        source,
    })?;

    if !status.success() {
        bail!("SSH session exited with {status}");
    }

    Ok(())
}

/// Run a command over SSH, capturing stdout and stderr.
///
/// Returns the combined output as a string. Fails with context if the
/// command exits non-zero. Use this instead of `session()` when the
/// output should be captured rather than forwarded to the terminal.
pub async fn run_cmd(
    instance: &Instance,
    user: &str,
    command: &[String],
) -> anyhow::Result<String> {
    let port = ssh_port(instance).await?;
    let key_path = instance.ssh_key_path();
    let args = base_ssh_args(&key_path, port);

    let destination = format!("{user}@localhost");

    let mut cmd = tokio::process::Command::new("ssh");
    cmd.args(&args).arg(&destination);

    if !command.is_empty() {
        cmd.arg("--");
        cmd.args(command);
    }

    let output = cmd.output().await.map_err(|source| Error::Ssh {
        name: instance.name.clone(),
        source,
    })?;

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

/// Copy a file into the VM using scp.
pub async fn copy_to(
    instance: &Instance,
    user: &str,
    local_path: &Path,
    remote_path: &str,
) -> anyhow::Result<()> {
    let port = ssh_port(instance).await?;
    let key_path = instance.ssh_key_path();

    let local_str = local_path
        .to_str()
        .context("local path is not valid UTF-8")?;

    let destination = format!("{user}@localhost:{remote_path}");

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

    let output = tokio::process::Command::new("scp")
        .args(&args)
        .output()
        .await
        .map_err(|source| Error::Scp {
            name: instance.name.clone(),
            source,
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("scp failed (exit {}): {stderr}", output.status);
    }

    Ok(())
}

/// Wait for SSH to become available on a VM, polling until ready.
///
/// Retries up to 60 times with 1-second intervals (60s total timeout).
pub async fn wait_for_ready(instance: &Instance, user: &str) -> anyhow::Result<()> {
    let port = ssh_port(instance).await?;
    let key_path = instance.ssh_key_path();
    let args = base_ssh_args(&key_path, port);

    let destination = format!("{user}@localhost");
    let start = std::time::Instant::now();

    info!(vm = %instance.name, "waiting for SSH to become ready");

    for attempt in 1..=60 {
        let output = tokio::process::Command::new("ssh")
            .args(&args)
            .arg(&destination)
            .arg("true")
            .output()
            .await;

        if output.is_ok_and(|o| o.status.success()) {
            let elapsed = start.elapsed();
            info!(
                vm = %instance.name,
                elapsed_secs = elapsed.as_secs(),
                "SSH ready after {attempt} attempt(s)"
            );
            return Ok(());
        }

        debug!(
            vm = %instance.name,
            attempt,
            "SSH not ready yet, retrying in 1s"
        );
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    Err(Error::SshTimeout {
        name: instance.name.clone(),
    }
    .into())
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Read the SSH port from the instance's `ssh_port` file.
async fn ssh_port(instance: &Instance) -> anyhow::Result<u16> {
    let path = instance.ssh_port_path();
    let raw = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("failed to read SSH port file {}", path.display()))?;

    raw.trim()
        .parse::<u16>()
        .with_context(|| format!("invalid SSH port in {}: {raw:?}", path.display()))
}

/// Build the common SSH arguments used by all operations.
fn base_ssh_args(key_path: &Path, port: u16) -> Vec<String> {
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
}
