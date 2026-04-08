//! Managed SSH config for IDE and tool integration.
//!
//! Maintains `<data_dir>/ssh_config` with a `Host` entry for each running VM.
//! Users add `Include <data_dir>/ssh_config` to their `~/.ssh/config` once
//! (via `agv doctor --setup-ssh`), then every running VM is accessible by
//! name from any SSH-based tool — IDEs, plain `ssh`, `rsync`, etc.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::dirs;

/// Marker comments so we can find and remove our Include line.
const MARKER_START: &str = "# --- agv managed (do not edit) ---";
const MARKER_END: &str = "# --- end agv ---";

/// Path to the agv-managed SSH config file.
pub fn managed_config_path() -> anyhow::Result<PathBuf> {
    Ok(dirs::data_dir()?.join("ssh_config"))
}

/// Path to the user's SSH config file.
fn user_ssh_config_path() -> anyhow::Result<PathBuf> {
    #[allow(deprecated)]
    let home = std::env::home_dir().context("could not determine home directory")?;
    Ok(home.join(".ssh/config"))
}

// ---------------------------------------------------------------------------
// Per-VM entry management (called by start/stop/destroy)
// ---------------------------------------------------------------------------

/// Add or update a Host entry for a VM in the managed SSH config.
pub async fn add_entry(name: &str, port: u16, user: &str, key_path: &Path) -> anyhow::Result<()> {
    let config_path = managed_config_path()?;
    let mut content = read_or_empty(&config_path).await;

    // Remove existing entry for this name, if any.
    content = remove_host_block(&content, name);

    let key_str = key_path.display();
    let entry = format!(
        "Host {name}\n\
         \x20   HostName localhost\n\
         \x20   Port {port}\n\
         \x20   User {user}\n\
         \x20   IdentityFile {key_str}\n\
         \x20   StrictHostKeyChecking no\n\
         \x20   UserKnownHostsFile /dev/null\n\
         \x20   LogLevel ERROR\n\n"
    );

    content.push_str(&entry);

    tokio::fs::write(&config_path, &content)
        .await
        .with_context(|| format!("failed to write {}", config_path.display()))
}

/// Remove a VM's Host entry from the managed SSH config.
pub async fn remove_entry(name: &str) -> anyhow::Result<()> {
    let config_path = managed_config_path()?;
    let content = read_or_empty(&config_path).await;
    let updated = remove_host_block(&content, name);

    tokio::fs::write(&config_path, &updated)
        .await
        .with_context(|| format!("failed to write {}", config_path.display()))
}

// ---------------------------------------------------------------------------
// Include line management (called by doctor --setup-ssh / --remove-ssh)
// ---------------------------------------------------------------------------

/// Check if the Include line is present in ~/.ssh/config.
pub fn is_include_installed() -> anyhow::Result<bool> {
    let ssh_config = user_ssh_config_path()?;
    if !ssh_config.exists() {
        return Ok(false);
    }
    let content = std::fs::read_to_string(&ssh_config)
        .with_context(|| format!("failed to read {}", ssh_config.display()))?;
    Ok(content.contains(MARKER_START))
}

/// Add the Include line to ~/.ssh/config, wrapped in marker comments.
pub fn install_include() -> anyhow::Result<()> {
    let ssh_config = user_ssh_config_path()?;
    let managed = managed_config_path()?;

    // Ensure ~/.ssh exists.
    if let Some(ssh_dir) = ssh_config.parent() {
        std::fs::create_dir_all(ssh_dir)
            .with_context(|| format!("failed to create {}", ssh_dir.display()))?;
    }

    // Check if already installed.
    if is_include_installed()? {
        println!("  SSH config Include is already set up.");
        return Ok(());
    }

    // Read existing content (if any).
    let existing = if ssh_config.exists() {
        std::fs::read_to_string(&ssh_config)
            .with_context(|| format!("failed to read {}", ssh_config.display()))?
    } else {
        String::new()
    };

    // The Include must be at the top of the file to take effect.
    let include_block = format!(
        "{MARKER_START}\nInclude {}\n{MARKER_END}\n",
        managed.display()
    );

    let new_content = if existing.is_empty() {
        include_block
    } else {
        format!("{include_block}\n{existing}")
    };

    std::fs::write(&ssh_config, &new_content)
        .with_context(|| format!("failed to write {}", ssh_config.display()))?;

    // Ensure the managed file exists (even if empty).
    if !managed.exists() {
        std::fs::write(&managed, "").with_context(|| {
            format!("failed to create {}", managed.display())
        })?;
    }

    println!("  Added Include to {}", ssh_config.display());
    println!("  Managed config: {}", managed.display());
    Ok(())
}

/// Remove the Include line from ~/.ssh/config.
pub fn remove_include() -> anyhow::Result<()> {
    let ssh_config = user_ssh_config_path()?;

    if !ssh_config.exists() {
        println!("  No ~/.ssh/config found — nothing to remove.");
        return Ok(());
    }

    let content = std::fs::read_to_string(&ssh_config)
        .with_context(|| format!("failed to read {}", ssh_config.display()))?;

    if !content.contains(MARKER_START) {
        println!("  No agv Include found in {} — nothing to remove.", ssh_config.display());
        return Ok(());
    }

    // Remove everything between (and including) the markers.
    let mut result = String::with_capacity(content.len());
    let mut in_block = false;
    for line in content.lines() {
        if line.trim() == MARKER_START {
            in_block = true;
            continue;
        }
        if line.trim() == MARKER_END {
            in_block = false;
            continue;
        }
        if !in_block {
            result.push_str(line);
            result.push('\n');
        }
    }

    // Remove leading blank lines left by the removal.
    let trimmed = result.trim_start_matches('\n');

    std::fs::write(&ssh_config, trimmed)
        .with_context(|| format!("failed to write {}", ssh_config.display()))?;

    println!("  Removed agv Include from {}", ssh_config.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn read_or_empty(path: &Path) -> String {
    tokio::fs::read_to_string(path).await.unwrap_or_default()
}

/// Remove a `Host <name>` block from SSH config content.
///
/// A block starts with `Host <name>` and ends at the next `Host` line or EOF.
fn remove_host_block(content: &str, name: &str) -> String {
    let host_line = format!("Host {name}");
    let mut result = String::with_capacity(content.len());
    let mut skipping = false;

    for line in content.lines() {
        if line.trim() == host_line {
            skipping = true;
            continue;
        }
        if skipping && line.starts_with("Host ") {
            skipping = false;
        }
        if !skipping {
            result.push_str(line);
            result.push('\n');
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_host_block_removes_target() {
        let content = "\
Host foo
    HostName localhost
    Port 2222

Host bar
    HostName localhost
    Port 3333
";
        let result = remove_host_block(content, "foo");
        assert!(!result.contains("Host foo"));
        assert!(!result.contains("Port 2222"));
        assert!(result.contains("Host bar"));
        assert!(result.contains("Port 3333"));
    }

    #[test]
    fn remove_host_block_no_match() {
        let content = "\
Host bar
    HostName localhost
    Port 3333
";
        let result = remove_host_block(content, "foo");
        assert!(result.contains("Host bar"));
        assert!(result.contains("Port 3333"));
    }

    #[test]
    fn remove_host_block_only_entry() {
        let content = "\
Host foo
    HostName localhost
    Port 2222
";
        let result = remove_host_block(content, "foo");
        assert!(!result.contains("Host foo"));
        assert!(!result.contains("Port 2222"));
    }

    #[test]
    fn remove_host_block_empty() {
        let result = remove_host_block("", "foo");
        assert_eq!(result, "");
    }

    #[test]
    fn managed_config_path_is_under_data_dir() {
        let path = managed_config_path().unwrap();
        let data = dirs::data_dir().unwrap();
        assert!(path.starts_with(&data));
        assert!(path.ends_with("ssh_config"));
    }
}
