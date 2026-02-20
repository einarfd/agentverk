//! TOML config parsing and merging with CLI flags.
//!
//! The config file format mirrors the `agv.toml` specification from the
//! design doc. CLI flags take precedence over config file values.

use std::path::Path;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use crate::cli::CreateArgs;

/// Root config structure, parsed from a TOML file.
#[derive(Debug, Default, Deserialize, Serialize)]
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
#[derive(Debug, Default, Deserialize, Serialize)]
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
#[derive(Debug, Deserialize, Serialize)]
pub struct FileEntry {
    /// Source path on the host. Supports `~` expansion; resolved relative
    /// to the config file's directory.
    pub source: String,

    /// Destination path inside the VM.
    pub dest: String,
}

/// A single provisioning step: either an inline script or a script file.
#[derive(Debug, Deserialize, Serialize)]
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

/// Build a merged config from CLI args and an optional config file.
///
/// Cascade order: CLI flag → config file → hardcoded default.
/// Returns `(name, config)`.
pub fn build_from_cli(args: &CreateArgs) -> anyhow::Result<(String, Config)> {
    // Load base config: explicit --config, or agv.toml if present, or default.
    let mut config = if let Some(ref path) = args.config {
        load(Path::new(path))?
    } else if Path::new("agv.toml").exists() {
        load(Path::new("agv.toml"))?
    } else {
        Config::default()
    };

    // Ensure vm section exists for overlay.
    let vm = config.vm.get_or_insert_with(VmConfig::default);

    // Overlay CLI values — only override when Some.
    if args.memory.is_some() {
        vm.memory.clone_from(&args.memory);
    }
    if args.cpus.is_some() {
        vm.cpus = args.cpus;
    }
    if args.disk.is_some() {
        vm.disk.clone_from(&args.disk);
    }
    if args.image.is_some() {
        vm.image.clone_from(&args.image);
    }
    if args.image_checksum.is_some() {
        vm.image_checksum.clone_from(&args.image_checksum);
    }

    // Parse --file src:dest strings into FileEntry structs.
    for raw in &args.files {
        let (source, dest) = raw.split_once(':').ok_or_else(|| {
            anyhow::anyhow!(
                "invalid --file format: {raw:?} — expected source:dest (e.g. ./setup.sh:/home/agent/setup.sh)"
            )
        })?;
        config.files.push(FileEntry {
            source: source.to_string(),
            dest: dest.to_string(),
        });
    }

    // Parse --provision inline scripts.
    for script in &args.provisions {
        config.provision.push(ProvisionStep {
            run: Some(script.clone()),
            script: None,
        });
    }

    // Parse --provision-script file paths.
    for path in &args.provision_scripts {
        config.provision.push(ProvisionStep {
            run: None,
            script: Some(path.clone()),
        });
    }

    // Resolve name: CLI --name → config file vm.name → error.
    let name = args
        .name
        .clone()
        .or_else(|| vm.name.clone())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "VM name is required — provide --name or set vm.name in the config file"
            )
        })?;

    Ok((name, config))
}

/// Serialize a config to TOML and write it to disk.
pub async fn save(config: &Config, path: &Path) -> anyhow::Result<()> {
    let toml_str = toml::to_string_pretty(config)
        .context("failed to serialize config to TOML")?;
    tokio::fs::write(path, toml_str)
        .await
        .with_context(|| format!("failed to write config to {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_args() -> CreateArgs {
        CreateArgs {
            config: None,
            name: Some("test-vm".to_string()),
            memory: None,
            cpus: None,
            disk: None,
            image: None,
            image_checksum: None,
            files: vec![],
            provisions: vec![],
            provision_scripts: vec![],
            start: false,
        }
    }

    #[test]
    fn build_from_cli_minimal() {
        let args = minimal_args();
        let (name, config) = build_from_cli(&args).unwrap();
        assert_eq!(name, "test-vm");
        // vm section exists but all values are None (defaults applied later in vm::create).
        let vm = config.vm.unwrap();
        assert!(vm.memory.is_none());
        assert!(vm.cpus.is_none());
        assert!(vm.disk.is_none());
    }

    #[test]
    fn build_from_cli_name_required() {
        let args = CreateArgs {
            name: None,
            ..minimal_args()
        };
        let result = build_from_cli(&args);
        assert!(result.is_err());
        let err = format!("{:#}", result.unwrap_err());
        assert!(err.contains("VM name is required"), "unexpected error: {err}");
    }

    #[test]
    fn build_from_cli_parses_files() {
        let args = CreateArgs {
            files: vec![
                "./setup.sh:/home/agent/setup.sh".to_string(),
                "/etc/hosts:/etc/hosts".to_string(),
            ],
            ..minimal_args()
        };
        let (_, config) = build_from_cli(&args).unwrap();
        assert_eq!(config.files.len(), 2);
        assert_eq!(config.files[0].source, "./setup.sh");
        assert_eq!(config.files[0].dest, "/home/agent/setup.sh");
        assert_eq!(config.files[1].source, "/etc/hosts");
        assert_eq!(config.files[1].dest, "/etc/hosts");
    }

    #[test]
    fn build_from_cli_invalid_file_format() {
        let args = CreateArgs {
            files: vec!["no-colon-here".to_string()],
            ..minimal_args()
        };
        let result = build_from_cli(&args);
        assert!(result.is_err());
        let err = format!("{:#}", result.unwrap_err());
        assert!(err.contains("invalid --file format"), "unexpected error: {err}");
    }

    #[test]
    fn build_from_cli_provisions() {
        let args = CreateArgs {
            provisions: vec!["apt-get update".to_string()],
            provision_scripts: vec!["./setup.sh".to_string()],
            ..minimal_args()
        };
        let (_, config) = build_from_cli(&args).unwrap();
        assert_eq!(config.provision.len(), 2);
        assert_eq!(
            config.provision[0].run.as_deref(),
            Some("apt-get update")
        );
        assert!(config.provision[0].script.is_none());
        assert!(config.provision[1].run.is_none());
        assert_eq!(
            config.provision[1].script.as_deref(),
            Some("./setup.sh")
        );
    }

    #[test]
    fn build_from_cli_with_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("agv.toml");
        std::fs::write(
            &config_path,
            r#"
[vm]
name = "from-config"
memory = "8G"
cpus = 4
"#,
        )
        .unwrap();

        let args = CreateArgs {
            config: Some(config_path.to_str().unwrap().to_string()),
            name: None,
            ..minimal_args()
        };
        let (name, config) = build_from_cli(&args).unwrap();
        assert_eq!(name, "from-config");
        let vm = config.vm.unwrap();
        assert_eq!(vm.memory.as_deref(), Some("8G"));
        assert_eq!(vm.cpus, Some(4));
    }

    #[test]
    fn build_from_cli_cli_overrides_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("agv.toml");
        std::fs::write(
            &config_path,
            r#"
[vm]
name = "from-config"
memory = "8G"
cpus = 4
"#,
        )
        .unwrap();

        let args = CreateArgs {
            config: Some(config_path.to_str().unwrap().to_string()),
            name: Some("from-cli".to_string()),
            memory: Some("16G".to_string()),
            cpus: None, // should keep config value
            ..minimal_args()
        };
        let (name, config) = build_from_cli(&args).unwrap();
        assert_eq!(name, "from-cli");
        let vm = config.vm.unwrap();
        assert_eq!(vm.memory.as_deref(), Some("16G"));
        assert_eq!(vm.cpus, Some(4)); // kept from config
    }

    #[tokio::test]
    async fn save_and_reload_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let config = Config {
            vm: Some(VmConfig {
                name: Some("round-trip".to_string()),
                memory: Some("4G".to_string()),
                cpus: Some(8),
                disk: Some("50G".to_string()),
                user: Some("testuser".to_string()),
                image: None,
                image_checksum: None,
            }),
            files: vec![FileEntry {
                source: "/tmp/src".to_string(),
                dest: "/home/agent/dst".to_string(),
            }],
            provision: vec![ProvisionStep {
                run: Some("echo hello".to_string()),
                script: None,
            }],
        };

        save(&config, &path).await.unwrap();
        let reloaded = load(&path).unwrap();

        let vm = reloaded.vm.unwrap();
        assert_eq!(vm.name.as_deref(), Some("round-trip"));
        assert_eq!(vm.memory.as_deref(), Some("4G"));
        assert_eq!(vm.cpus, Some(8));
        assert_eq!(vm.disk.as_deref(), Some("50G"));
        assert_eq!(vm.user.as_deref(), Some("testuser"));

        assert_eq!(reloaded.files.len(), 1);
        assert_eq!(reloaded.files[0].source, "/tmp/src");
        assert_eq!(reloaded.files[0].dest, "/home/agent/dst");

        assert_eq!(reloaded.provision.len(), 1);
        assert_eq!(
            reloaded.provision[0].run.as_deref(),
            Some("echo hello")
        );
    }
}
