//! Cloud-init seed image generation.
//!
//! Builds an ISO seed image containing user-data and meta-data for
//! first-boot configuration: SSH keys, hostname, and user setup.

use std::path::Path;

use anyhow::{bail, Context as _};
use tracing::info;

/// Default username for the VM's primary account.
pub const DEFAULT_USER: &str = "agent";

/// Generate a cloud-init seed ISO at the given output path.
///
/// The seed image contains:
/// - `authorized_keys` with the generated SSH public key
/// - `hostname` set to the VM name
/// - Basic user setup (default user with sudo access)
///
/// Note: `[[files]]` are copied via SCP after SSH is ready, not via
/// cloud-init `write_files`, to avoid silent failures and ownership issues.
pub async fn generate_seed(
    output: &Path,
    ssh_pub_key: &str,
    vm_name: &str,
    user: &str,
) -> anyhow::Result<()> {
    let parent = output
        .parent()
        .context("seed output path has no parent directory")?;
    let staging = parent.join("seed-staging");
    tokio::fs::create_dir_all(&staging)
        .await
        .with_context(|| format!("failed to create staging directory {}", staging.display()))?;

    let meta_data = render_meta_data(vm_name);
    let user_data = render_user_data(ssh_pub_key, vm_name, user).await?;

    tokio::fs::write(staging.join("meta-data"), &meta_data)
        .await
        .context("failed to write meta-data")?;
    tokio::fs::write(staging.join("user-data"), &user_data)
        .await
        .context("failed to write user-data")?;

    let output_str = output
        .to_str()
        .context("seed output path is not valid UTF-8")?;

    info!(output = output_str, "generating seed ISO");

    let result = run_iso_tool(output_str, &staging).await;

    // Clean up staging dir regardless of outcome.
    let _ = tokio::fs::remove_dir_all(&staging).await;

    result?;
    info!(path = output_str, "seed ISO created");
    Ok(())
}

/// Render the cloud-init `meta-data` YAML.
fn render_meta_data(vm_name: &str) -> String {
    format!("instance-id: {vm_name}\nlocal-hostname: {vm_name}\n")
}

/// Render the cloud-init `user-data` cloud-config YAML.
async fn render_user_data(
    ssh_pub_key: &str,
    vm_name: &str,
    user: &str,
) -> anyhow::Result<String> {
    let yaml = format!(
        "#cloud-config\n\
         hostname: {vm_name}\n\
         users:\n\
         \x20 - name: {user}\n\
         \x20   sudo: ALL=(ALL) NOPASSWD:ALL\n\
         \x20   shell: /bin/bash\n\
         \x20   ssh_authorized_keys:\n\
         \x20     - {ssh_pub_key}\n"
    );

    Ok(yaml)
}

// ---------------------------------------------------------------------------
// Platform-specific ISO creation
// ---------------------------------------------------------------------------

/// Create the seed ISO using `hdiutil makehybrid` (macOS built-in).
#[cfg(target_os = "macos")]
async fn run_iso_tool(output: &str, staging: &std::path::Path) -> anyhow::Result<()> {
    let staging_str = staging
        .to_str()
        .context("staging path is not valid UTF-8")?;

    let result = tokio::process::Command::new("hdiutil")
        .args([
            "makehybrid",
            "-o", output,
            "-iso",
            "-joliet",
            "-default-volume-name", "cidata",
            "-ov",
            "-quiet",
            staging_str,
        ])
        .output()
        .await;

    match result {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!("hdiutil failed (exit {}): {stderr}", out.status);
        }
        Err(e) => Err(e).context("failed to run hdiutil")?,
    }
}

/// Create the seed ISO using `mkisofs` or `genisoimage` (Linux).
#[cfg(not(target_os = "macos"))]
async fn run_iso_tool(output: &str, staging: &std::path::Path) -> anyhow::Result<()> {
    let tool = find_iso_tool().await?;

    let meta_str = staging
        .join("meta-data")
        .to_str()
        .context("meta-data path is not valid UTF-8")?
        .to_string();
    let user_str = staging
        .join("user-data")
        .to_str()
        .context("user-data path is not valid UTF-8")?
        .to_string();

    let result = tokio::process::Command::new(tool)
        .args([
            "-output", output, "-volid", "cidata", "-joliet", "-rock", &meta_str, &user_str,
        ])
        .output()
        .await;

    match result {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!("{tool} failed (exit {}): {stderr}", out.status);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("ISO creation tool not found — run 'agv doctor' to check all dependencies");
        }
        Err(e) => Err(e).with_context(|| format!("failed to run {tool}"))?,
    }
}

/// Find an available ISO-generation tool (`mkisofs` or `genisoimage`).
#[cfg(not(target_os = "macos"))]
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
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("failed to check for {tool}")),
        }
    }

    bail!("ISO creation tool not found — run 'agv doctor' to check all dependencies");
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
    async fn user_data_contains_expected_sections() {
        let output = render_user_data("ssh-ed25519 AAAA...", "my-vm", "agent")
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

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn iso_tool_is_hdiutil_on_macos() {
        // hdiutil is built into macOS — verify it's present and responsive.
        let out = tokio::process::Command::new("hdiutil")
            .arg("help")
            .output()
            .await
            .unwrap();
        assert!(out.status.success(), "hdiutil help failed");
    }

    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn find_iso_tool_returns_known_tool() {
        let tool = find_iso_tool().await.unwrap();
        assert!(tool == "mkisofs" || tool == "genisoimage");
    }
}
