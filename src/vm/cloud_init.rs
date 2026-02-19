//! Cloud-init seed image generation.
//!
//! Builds an ISO seed image containing user-data and meta-data for
//! first-boot configuration: SSH keys, hostname, files, and user setup.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{bail, Context as _};
use base64::Engine as _;
use tracing::info;

/// Default username for the VM's primary account.
pub const DEFAULT_USER: &str = "agent";

/// Generate a cloud-init seed ISO at the given output path.
///
/// The seed image contains:
/// - `authorized_keys` with the generated SSH public key
/// - `hostname` set to the VM name
/// - `write_files` entries for all files to inject
/// - Basic user setup (default user with sudo access)
pub async fn generate_seed(
    output: &Path,
    ssh_pub_key: &str,
    vm_name: &str,
    user: &str,
    files: &[(String, String)],
) -> anyhow::Result<()> {
    let parent = output
        .parent()
        .context("seed output path has no parent directory")?;
    let staging = parent.join("seed-staging");
    tokio::fs::create_dir_all(&staging)
        .await
        .with_context(|| format!("failed to create staging directory {}", staging.display()))?;

    let meta_data = render_meta_data(vm_name);
    let user_data = render_user_data(ssh_pub_key, vm_name, user, files).await?;

    tokio::fs::write(staging.join("meta-data"), &meta_data)
        .await
        .context("failed to write meta-data")?;
    tokio::fs::write(staging.join("user-data"), &user_data)
        .await
        .context("failed to write user-data")?;

    let tool = find_iso_tool().await?;

    let output_str = output
        .to_str()
        .context("seed output path is not valid UTF-8")?;
    let meta_path = staging.join("meta-data");
    let user_path = staging.join("user-data");
    let meta_str = meta_path
        .to_str()
        .context("meta-data path is not valid UTF-8")?;
    let user_str = user_path
        .to_str()
        .context("user-data path is not valid UTF-8")?;

    info!(tool = tool, output = output_str, "generating seed ISO");

    let result = tokio::process::Command::new(tool)
        .args([
            "-output", output_str, "-volid", "cidata", "-joliet", "-rock", meta_str, user_str,
        ])
        .output()
        .await;

    // Clean up staging dir regardless of outcome.
    let _ = tokio::fs::remove_dir_all(&staging).await;

    match result {
        Ok(out) if out.status.success() => {
            info!(path = output_str, "seed ISO created");
            Ok(())
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!("{tool} failed (exit {}): {stderr}", out.status);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!(
                "neither mkisofs nor genisoimage found — \
                 install cdrtools (macOS: `brew install cdrtools`) \
                 or genisoimage (Linux: `apt install genisoimage`)"
            );
        }
        Err(e) => {
            Err(e).with_context(|| format!("failed to run {tool}"))?
        }
    }
}

/// Render the cloud-init `meta-data` YAML.
fn render_meta_data(vm_name: &str) -> String {
    format!("instance-id: {vm_name}\nlocal-hostname: {vm_name}\n")
}

/// Render the cloud-init `user-data` cloud-config YAML.
///
/// Reads each source file from the host and base64-encodes it for
/// the `write_files` section.
async fn render_user_data(
    ssh_pub_key: &str,
    vm_name: &str,
    user: &str,
    files: &[(String, String)],
) -> anyhow::Result<String> {
    let mut yaml = format!(
        "#cloud-config\n\
         hostname: {vm_name}\n\
         users:\n\
         \x20 - name: {user}\n\
         \x20   sudo: ALL=(ALL) NOPASSWD:ALL\n\
         \x20   shell: /bin/bash\n\
         \x20   ssh_authorized_keys:\n\
         \x20     - {ssh_pub_key}\n"
    );

    if !files.is_empty() {
        yaml.push_str("write_files:\n");
        for (source, dest) in files {
            let content = tokio::fs::read(source).await.with_context(|| {
                format!("failed to read file `{source}` for injection into VM")
            })?;
            let encoded = base64::engine::general_purpose::STANDARD.encode(&content);
            let _ = write!(
                yaml,
                "  - path: {dest}\n\
                 \x20   encoding: b64\n\
                 \x20   content: {encoded}\n\
                 \x20   owner: {user}:{user}\n\
                 \x20   permissions: '0644'\n"
            );
        }
    }

    Ok(yaml)
}

/// Find an available ISO-generation tool (`mkisofs` or `genisoimage`).
async fn find_iso_tool() -> anyhow::Result<&'static str> {
    for tool in ["mkisofs", "genisoimage"] {
        let result = tokio::process::Command::new(tool)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;

        match result {
            Ok(_) => return Ok(tool),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
            Err(e) => return Err(e).with_context(|| format!("failed to check for {tool}")),
        }
    }

    bail!(
        "neither mkisofs nor genisoimage found — \
         install cdrtools (macOS: `brew install cdrtools`) \
         or genisoimage (Linux: `apt install genisoimage`)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_data_contains_instance_id_and_hostname() {
        let output = render_meta_data("test-vm");
        assert!(output.contains("instance-id: test-vm"));
        assert!(output.contains("local-hostname: test-vm"));
    }

    #[tokio::test]
    async fn user_data_no_files() {
        let output = render_user_data("ssh-ed25519 AAAA...", "my-vm", "agent", &[])
            .await
            .unwrap();
        assert!(output.starts_with("#cloud-config"));
        assert!(output.contains("hostname: my-vm"));
        assert!(output.contains("name: agent"));
        assert!(output.contains("sudo: ALL=(ALL) NOPASSWD:ALL"));
        assert!(output.contains("shell: /bin/bash"));
        assert!(output.contains("ssh-ed25519 AAAA..."));
        assert!(!output.contains("write_files"));
    }

    #[tokio::test]
    async fn user_data_with_files() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("hello.txt");
        tokio::fs::write(&src, b"Hello, world!")
            .await
            .unwrap();
        let src_str = src.to_str().unwrap().to_string();

        let files = vec![(src_str, "/home/agent/hello.txt".to_string())];
        let output = render_user_data("ssh-ed25519 AAAA...", "my-vm", "agent", &files)
            .await
            .unwrap();

        assert!(output.contains("write_files:"));
        assert!(output.contains("path: /home/agent/hello.txt"));
        assert!(output.contains("encoding: b64"));
        assert!(output.contains("owner: agent:agent"));
        assert!(output.contains("permissions: '0644'"));

        // Verify base64 content decodes to original.
        let encoded = base64::engine::general_purpose::STANDARD.encode(b"Hello, world!");
        assert!(output.contains(&encoded));
    }

    #[tokio::test]
    async fn user_data_missing_source_file_errors() {
        let files = vec![("/nonexistent/file.txt".to_string(), "/dest".to_string())];
        let result = render_user_data("ssh-ed25519 AAAA...", "my-vm", "agent", &files).await;
        assert!(result.is_err());
        let err = format!("{:#}", result.unwrap_err());
        assert!(err.contains("failed to read file `/nonexistent/file.txt`"));
    }

    #[tokio::test]
    async fn find_iso_tool_returns_known_tool() {
        let tool = find_iso_tool().await.unwrap();
        assert!(tool == "mkisofs" || tool == "genisoimage");
    }
}
