//! Platform-specific state and cache directory paths.
//!
//! On macOS: `~/Library/Application Support/agv/`
//! On Linux: `~/.local/share/agv/`

use std::path::PathBuf;

use anyhow::Context as _;

/// Return the root data directory for agv.
///
/// - macOS: `~/Library/Application Support/agv/`
/// - Linux: `~/.local/share/agv/`
pub fn data_dir() -> anyhow::Result<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        home_dir()?.join("Library/Application Support")
    } else {
        home_dir()?.join(".local/share")
    };
    Ok(base.join("agv"))
}

/// Return the directory where downloaded images are cached.
pub fn image_cache_dir() -> anyhow::Result<PathBuf> {
    Ok(data_dir()?.join("cache/images"))
}

/// Return the directory containing all VM instance state.
pub fn instances_dir() -> anyhow::Result<PathBuf> {
    Ok(data_dir()?.join("instances"))
}

/// Return the state directory for a specific VM instance.
pub fn instance_dir(name: &str) -> anyhow::Result<PathBuf> {
    Ok(instances_dir()?.join(name))
}

fn home_dir() -> anyhow::Result<PathBuf> {
    #[allow(deprecated)]
    std::env::home_dir().context("could not determine home directory")
}

/// Ensure the core directory structure exists.
pub async fn ensure_dirs() -> anyhow::Result<()> {
    let dirs = [image_cache_dir()?, instances_dir()?];
    for dir in &dirs {
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("failed to create directory {}", dir.display()))?;
    }
    Ok(())
}
