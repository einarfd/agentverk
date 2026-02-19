//! TOML config parsing and merging with CLI flags.
//!
//! The config file format mirrors the `agv.toml` specification from the
//! design doc. CLI flags take precedence over config file values.

use std::path::Path;

use anyhow::Context as _;
use serde::Deserialize;

/// Root config structure, parsed from a TOML file.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// VM settings.
    pub vm: Option<VmConfig>,

    /// Files to copy into the VM before provisioning.
    #[serde(default)]
    pub files: Vec<FileEntry>,

    /// Provisioning steps, executed in order after files are copied.
    #[serde(default)]
    pub provision: Vec<ProvisionStep>,
}

/// VM resource and identity configuration.
#[derive(Debug, Deserialize)]
pub struct VmConfig {
    /// VM name.
    pub name: Option<String>,

    /// Memory allocation, e.g. "4G", "512M".
    pub memory: Option<String>,

    /// Number of virtual CPUs.
    pub cpus: Option<u32>,

    /// Disk size, e.g. "20G".
    pub disk: Option<String>,

    /// Username for the VM's default user. Defaults to "agent".
    pub user: Option<String>,

    /// Base image URL (qcow2 cloud image).
    pub image: Option<String>,

    /// SHA256 checksum for image verification, format: `sha256:<hex>`.
    pub image_checksum: Option<String>,
}

/// A file or directory to copy into the VM.
#[derive(Debug, Deserialize)]
pub struct FileEntry {
    /// Source path on the host. Supports `~` expansion; resolved relative
    /// to the config file's directory.
    pub source: String,

    /// Destination path inside the VM.
    pub dest: String,
}

/// A single provisioning step: either an inline script or a script file.
#[derive(Debug, Deserialize)]
pub struct ProvisionStep {
    /// Inline shell script to execute inside the VM.
    pub run: Option<String>,

    /// Path to a script file to copy into the VM and execute.
    /// Resolved relative to the config file's directory.
    pub script: Option<String>,
}

/// Load and parse a config file from the given path.
pub fn load(path: &Path) -> anyhow::Result<Config> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let config: Config = toml::from_str(&contents)
        .with_context(|| format!("failed to parse config file {}", path.display()))?;
    Ok(config)
}
