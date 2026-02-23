//! TOML config parsing, image inheritance resolution, and CLI merging.
//!
//! Image definitions form an inheritance chain: a derived image references a
//! parent via `base.from`, and scalars override while lists accumulate.
//! Resolution flattens the chain into a `ResolvedConfig` with no Options.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{bail, Context as _};
use serde::{Deserialize, Serialize};

use crate::cli::CreateArgs;
use crate::dirs;
use crate::error::Error;

// ---------------------------------------------------------------------------
// Raw config structs (parsed from TOML)
// ---------------------------------------------------------------------------

/// Root config structure, parsed from a TOML file or image definition.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Image base: inherits from another image or specifies arch-specific URLs.
    pub base: Option<BaseConfig>,

    /// VM resource settings.
    pub vm: Option<VmConfig>,

    /// Named modules to include (additive files/setup/provision steps).
    #[serde(default)]
    pub include: Vec<String>,

    /// Files to copy into the VM before provisioning.
    #[serde(default)]
    pub files: Vec<FileEntry>,

    /// Setup steps, executed as root before provisioning.
    #[serde(default)]
    pub setup: Vec<ProvisionStep>,

    /// Provisioning steps, executed in order after files are copied.
    #[serde(default)]
    pub provision: Vec<ProvisionStep>,
}

/// Image source — either a parent image name or arch-specific cloud image URLs.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct BaseConfig {
    /// Parent image name to inherit from (derived images).
    pub from: Option<String>,

    /// ARM64 cloud image (root images only).
    pub aarch64: Option<ArchImage>,

    /// `x86_64` cloud image (root images only).
    pub x86_64: Option<ArchImage>,
}

/// Per-architecture cloud image definition.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ArchImage {
    /// Cloud image URL for this architecture.
    pub url: String,

    /// SHA256 checksum, format: `sha256:<hex>`.
    pub checksum: String,
}

/// VM resource configuration — all fields optional for merging.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct VmConfig {
    /// Memory allocation, e.g. "4G", "512M".
    pub memory: Option<String>,

    /// Number of virtual CPUs.
    pub cpus: Option<u32>,

    /// Disk size, e.g. "20G".
    pub disk: Option<String>,

    /// Username for the VM's default user. Defaults to "agent".
    pub user: Option<String>,
}

/// A file or directory to copy into the VM.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FileEntry {
    /// Source path on the host.
    pub source: String,

    /// Destination path inside the VM.
    pub dest: String,
}

/// A single provisioning step: either an inline script or a script file.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProvisionStep {
    /// Which module/include contributed this step.
    ///
    /// Auto-populated during resolution — users never write this.
    /// Preserved in the saved resolved config so `agv start` first-boot
    /// can still display the source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,

    /// Inline shell script to execute inside the VM.
    pub run: Option<String>,

    /// Path to a script file to copy into the VM and execute.
    pub script: Option<String>,
}

// ---------------------------------------------------------------------------
// Resolved config — fully flattened, no Options
// ---------------------------------------------------------------------------

/// A fully resolved config with no inheritance and no Option fields.
///
/// Produced by [`resolve()`] after flattening the entire inheritance chain.
/// Saved to and loaded from instance config files.
#[derive(Debug, Deserialize, Serialize)]
pub struct ResolvedConfig {
    /// Base image URL for the current architecture.
    pub base_url: String,

    /// SHA256 checksum for the base image.
    pub base_checksum: String,

    /// Skip checksum verification (set via `--no-checksum`).
    #[serde(default)]
    pub skip_checksum: bool,

    /// Memory allocation, e.g. "2G".
    pub memory: String,

    /// Number of virtual CPUs.
    pub cpus: u32,

    /// Disk size, e.g. "20G".
    pub disk: String,

    /// Username for the VM's default user.
    pub user: String,

    /// Files to copy into the VM (accumulated from full chain).
    #[serde(default)]
    pub files: Vec<FileEntry>,

    /// Setup steps run as root (accumulated from full chain).
    #[serde(default)]
    pub setup: Vec<ProvisionStep>,

    /// Provisioning steps (accumulated from full chain).
    #[serde(default)]
    pub provision: Vec<ProvisionStep>,

    /// Name of the template this VM was cloned from, if any.
    ///
    /// Set when a VM is created with `agv create --from <template>`.
    /// Used by `inspect` to show template origin instead of a base image URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_name: Option<String>,
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// Resolve a `Config` into a fully flattened `ResolvedConfig`.
///
/// If the config has `base.from`, looks up the parent image, resolves it
/// recursively, and merges the child on top. Root images (no `from`) must
/// have an arch-specific URL for the current architecture.
pub fn resolve(config: Config) -> anyhow::Result<ResolvedConfig> {
    resolve_inner(config, &mut HashSet::new())
}

fn resolve_inner(config: Config, seen: &mut HashSet<String>) -> anyhow::Result<ResolvedConfig> {
    // Extract `from` as an owned value before destructuring config.
    let from = config
        .base
        .as_ref()
        .and_then(|b| b.from.clone());

    // Destructure config to avoid partial-move issues.
    let Config {
        base,
        vm,
        include: child_includes,
        files: child_files,
        setup: child_setup,
        provision: child_provision,
    } = config;

    if let Some(parent_name) = from {
        // Circular detection.
        if !seen.insert(parent_name.clone()) {
            let chain = seen
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(" -> ");
            return Err(Error::CircularInheritance {
                chain: format!("{chain} -> {parent_name}"),
            }
            .into());
        }

        // Look up the parent image.
        let parent_config =
            crate::images::lookup(&parent_name)?.ok_or_else(|| Error::ImageNotFound {
                name: parent_name.clone(),
                dir: dirs::images_dir().unwrap_or_default(),
            })?;

        // Resolve parent recursively.
        let parent_resolved = resolve_inner(parent_config, seen)?;

        // Build a child config with only scalars (no lists) for merging.
        let scalars_only = Config {
            base,
            vm,
            include: vec![],
            files: vec![],
            setup: vec![],
            provision: vec![],
        };

        // Merge child scalars on top of resolved parent.
        let mut resolved = merge(parent_resolved, scalars_only);

        // Apply includes (their steps come after parent, before child).
        apply_includes(&mut resolved, &child_includes, &mut HashSet::new())?;

        // Append the child's own steps last.
        resolved.files.extend(child_files);
        resolved.setup.extend(child_setup);
        resolved.provision.extend(child_provision);

        Ok(resolved)
    } else {
        // Root image — pick arch-specific URL.
        let base = base.context("root image config must have a [base] section")?;
        let arch = std::env::consts::ARCH;

        let arch_image = match arch {
            "aarch64" => base
                .aarch64
                .as_ref()
                .with_context(|| format!("no base image URL for architecture {arch}"))?,
            "x86_64" => base
                .x86_64
                .as_ref()
                .with_context(|| format!("no base image URL for architecture {arch}"))?,
            _ => bail!("unsupported architecture: {arch}"),
        };

        let mut resolved = ResolvedConfig {
            base_url: arch_image.url.clone(),
            base_checksum: arch_image.checksum.clone(),
            skip_checksum: false,
            memory: vm
                .as_ref()
                .and_then(|v| v.memory.clone())
                .unwrap_or_else(|| "2G".to_string()),
            cpus: vm.as_ref().and_then(|v| v.cpus).unwrap_or(2),
            disk: vm
                .as_ref()
                .and_then(|v| v.disk.clone())
                .unwrap_or_else(|| "20G".to_string()),
            user: vm
                .as_ref()
                .and_then(|v| v.user.clone())
                .unwrap_or_else(|| "agent".to_string()),
            files: vec![],
            setup: vec![],
            provision: vec![],
            template_name: None,
        };

        // Apply includes before the config's own steps.
        apply_includes(&mut resolved, &child_includes, &mut HashSet::new())?;

        // Append the config's own steps last.
        resolved.files.extend(child_files);
        resolved.setup.extend(child_setup);
        resolved.provision.extend(child_provision);

        Ok(resolved)
    }
}

/// Apply include modules to a resolved config.
///
/// Each include is looked up via `images::lookup()`. Includes contribute only
/// `files`, `setup`, and `provision` steps — they must NOT set `base.from`,
/// `base.aarch64/x86_64`, or `[vm]`. Include resolution is recursive
/// (includes can themselves have includes), with circular detection.
fn apply_includes(
    resolved: &mut ResolvedConfig,
    includes: &[String],
    seen: &mut HashSet<String>,
) -> anyhow::Result<()> {
    for name in includes {
        // Circular include detection.
        if !seen.insert(name.clone()) {
            let chain = seen
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(" -> ");
            return Err(Error::CircularInclude {
                chain: format!("{chain} -> {name}"),
            }
            .into());
        }

        let mut include_config =
            crate::images::lookup(name)?.ok_or_else(|| Error::InvalidInclude {
                name: name.clone(),
            })?;

        // Validate: includes must not set base.from, arch images, or vm settings.
        if let Some(ref base) = include_config.base {
            if base.from.is_some() || base.aarch64.is_some() || base.x86_64.is_some() {
                bail!(
                    "include '{name}' must not set base.from, base.aarch64, or base.x86_64 — includes contribute only files, setup, and provision steps"
                );
            }
        }
        if include_config.vm.is_some() {
            bail!(
                "include '{name}' must not set [vm] — includes contribute only files, setup, and provision steps"
            );
        }

        // Recursively resolve nested includes first.
        apply_includes(resolved, &include_config.include, seen)?;

        // Tag steps with the source module so status output can show origin.
        for step in &mut include_config.setup {
            if step.source.is_none() {
                step.source = Some(name.clone());
            }
        }
        for step in &mut include_config.provision {
            if step.source.is_none() {
                step.source = Some(name.clone());
            }
        }

        // Append this include's steps.
        resolved.files.extend(include_config.files);
        resolved.setup.extend(include_config.setup);
        resolved.provision.extend(include_config.provision);
    }

    Ok(())
}

/// Merge a resolved parent config with a child `Config`.
///
/// Scalars: child overrides parent if `Some`.
/// Lists (`files`, `provision`): parent first, then child.
/// `base_url`/`base_checksum`: always from parent (root).
fn merge(parent: ResolvedConfig, child: Config) -> ResolvedConfig {
    let vm = child.vm.as_ref();

    let mut files = parent.files;
    files.extend(child.files);

    let mut setup = parent.setup;
    setup.extend(child.setup);

    let mut provision = parent.provision;
    provision.extend(child.provision);

    ResolvedConfig {
        base_url: parent.base_url,
        base_checksum: parent.base_checksum,
        skip_checksum: parent.skip_checksum,
        memory: vm
            .and_then(|v| v.memory.clone())
            .unwrap_or(parent.memory),
        cpus: vm.and_then(|v| v.cpus).unwrap_or(parent.cpus),
        disk: vm.and_then(|v| v.disk.clone()).unwrap_or(parent.disk),
        user: vm.and_then(|v| v.user.clone()).unwrap_or(parent.user),
        files,
        setup,
        provision,
        template_name: None,
    }
}

// ---------------------------------------------------------------------------
// Loading and saving
// ---------------------------------------------------------------------------

/// Load and parse a config file from the given path.
pub fn load(path: &Path) -> anyhow::Result<Config> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let config: Config = toml::from_str(&contents)
        .with_context(|| format!("failed to parse config file {}", path.display()))?;
    Ok(config)
}

/// Load a resolved config from an instance's saved config file.
pub fn load_resolved(path: &Path) -> anyhow::Result<ResolvedConfig> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let config: ResolvedConfig = toml::from_str(&contents)
        .with_context(|| format!("failed to parse config file {}", path.display()))?;
    Ok(config)
}

/// Serialize a resolved config to TOML and write it to disk.
pub async fn save(config: &ResolvedConfig, path: &Path) -> anyhow::Result<()> {
    let toml_str =
        toml::to_string_pretty(config).context("failed to serialize config to TOML")?;
    tokio::fs::write(path, toml_str)
        .await
        .with_context(|| format!("failed to write config to {}", path.display()))
}

// ---------------------------------------------------------------------------
// CLI integration
// ---------------------------------------------------------------------------

/// Build a resolved config from CLI args.
///
/// Precedence for image source:
///   `--config <path>` > `--image <name>` > `agv.toml` (if exists) > `ubuntu-24.04`
pub fn build_from_cli(args: &CreateArgs) -> anyhow::Result<ResolvedConfig> {
    // 1. Determine the base config source.
    //    Also record the config file's directory so we can look for .env there.
    let mut config_dir: Option<std::path::PathBuf> = None;
    let mut config = if let Some(ref path) = args.config {
        let p = Path::new(path);
        config_dir = p.parent().map(std::path::Path::to_path_buf);
        load(p)?
    } else if let Some(ref image_name) = args.image {
        Config {
            base: Some(BaseConfig {
                from: Some(image_name.clone()),
                ..Default::default()
            }),
            ..Default::default()
        }
    } else if Path::new("agv.toml").exists() {
        // agv.toml is in cwd — .env next to it is already handled by cwd lookup.
        load(Path::new("agv.toml"))?
    } else {
        Config {
            base: Some(BaseConfig {
                from: Some("ubuntu-24.04".into()),
                ..Default::default()
            }),
            ..Default::default()
        }
    };

    // 2. Overlay CLI resource flags onto config before resolution.
    let vm = config.vm.get_or_insert_with(VmConfig::default);
    if args.memory.is_some() {
        vm.memory.clone_from(&args.memory);
    }
    if args.cpus.is_some() {
        vm.cpus = args.cpus;
    }
    if args.disk.is_some() {
        vm.disk.clone_from(&args.disk);
    }

    // 3. Parse --file src:dest strings into FileEntry structs.
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

    // 4. Parse --setup inline scripts.
    for script in &args.setups {
        config.setup.push(ProvisionStep {
            source: None,
            run: Some(script.clone()),
            script: None,
        });
    }

    // 5. Parse --setup-script file paths.
    for path in &args.setup_scripts {
        config.setup.push(ProvisionStep {
            source: None,
            run: None,
            script: Some(path.clone()),
        });
    }

    // 6. Parse --provision inline scripts.
    for script in &args.provisions {
        config.provision.push(ProvisionStep {
            source: None,
            run: Some(script.clone()),
            script: None,
        });
    }

    // 7. Parse --provision-script file paths.
    for path in &args.provision_scripts {
        config.provision.push(ProvisionStep {
            source: None,
            run: None,
            script: Some(path.clone()),
        });
    }

    // 8. Append CLI --include flags to config includes.
    config.include.extend(args.includes.clone());

    // 9. Resolve the full inheritance chain.
    let mut resolved = resolve(config)?;

    // 10. Expand template variables ({{VAR}} and {{VAR:-default}}).
    let mut vars = crate::template::load_variables(config_dir.as_deref());
    vars.insert("AGV_USER".to_string(), resolved.user.clone());
    crate::template::expand_config(&mut resolved, &vars)?;

    // 11. Apply --no-checksum flag.
    if args.no_checksum {
        resolved.skip_checksum = true;
    }

    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_args() -> CreateArgs {
        CreateArgs {
            config: None,
            name: "test-vm".to_string(),
            memory: None,
            cpus: None,
            disk: None,
            image: None,
            includes: vec![],
            files: vec![],
            setups: vec![],
            setup_scripts: vec![],
            provisions: vec![],
            provision_scripts: vec![],
            no_checksum: false,
            start: false,
            from: None,
        }
    }

    #[test]
    fn resolve_root_image() {
        let config = Config {
            base: Some(BaseConfig {
                from: None,
                aarch64: Some(ArchImage {
                    url: "https://example.com/arm64.img".to_string(),
                    checksum: "sha256:abc123".to_string(),
                }),
                x86_64: Some(ArchImage {
                    url: "https://example.com/amd64.img".to_string(),
                    checksum: "sha256:def456".to_string(),
                }),
            }),
            vm: Some(VmConfig {
                memory: Some("4G".to_string()),
                cpus: Some(4),
                disk: Some("30G".to_string()),
                user: Some("testuser".to_string()),
            }),
            include: vec![],
            files: vec![],
            setup: vec![],
            provision: vec![],
        };

        let resolved = resolve(config).unwrap();

        // Should pick the arch-appropriate URL.
        let arch = std::env::consts::ARCH;
        if arch == "aarch64" {
            assert_eq!(resolved.base_url, "https://example.com/arm64.img");
            assert_eq!(resolved.base_checksum, "sha256:abc123");
        } else {
            assert_eq!(resolved.base_url, "https://example.com/amd64.img");
            assert_eq!(resolved.base_checksum, "sha256:def456");
        }

        assert!(!resolved.skip_checksum);
        assert_eq!(resolved.memory, "4G");
        assert_eq!(resolved.cpus, 4);
        assert_eq!(resolved.disk, "30G");
        assert_eq!(resolved.user, "testuser");
    }

    #[test]
    fn resolve_root_defaults() {
        let config = Config {
            base: Some(BaseConfig {
                from: None,
                aarch64: Some(ArchImage {
                    url: "https://example.com/arm64.img".to_string(),
                    checksum: "sha256:aaa".to_string(),
                }),
                x86_64: Some(ArchImage {
                    url: "https://example.com/amd64.img".to_string(),
                    checksum: "sha256:bbb".to_string(),
                }),
            }),
            vm: None,
            include: vec![],
            files: vec![],
            setup: vec![],
            provision: vec![],
        };

        let resolved = resolve(config).unwrap();
        assert_eq!(resolved.memory, "2G");
        assert_eq!(resolved.cpus, 2);
        assert_eq!(resolved.disk, "20G");
        assert_eq!(resolved.user, "agent");
    }

    #[test]
    fn resolve_two_layers() {
        // This test resolves "ubuntu-24.04" (built-in) with child overrides.
        let child = Config {
            base: Some(BaseConfig {
                from: Some("ubuntu-24.04".to_string()),
                ..Default::default()
            }),
            vm: Some(VmConfig {
                memory: Some("8G".to_string()),
                cpus: Some(4),
                disk: None,
                user: None,
            }),
            include: vec![],
            files: vec![FileEntry {
                source: "./child-file".to_string(),
                dest: "/home/agent/child".to_string(),
            }],
            setup: vec![],
            provision: vec![ProvisionStep {
                source: None,
                run: Some("echo child".to_string()),
                script: None,
            }],
        };

        let resolved = resolve(child).unwrap();
        assert_eq!(resolved.memory, "8G");
        assert_eq!(resolved.cpus, 4);
        assert_eq!(resolved.disk, "20G"); // from ubuntu-24.04
        assert_eq!(resolved.user, "agent"); // from ubuntu-24.04
        assert_eq!(resolved.files.len(), 1);
        assert_eq!(resolved.provision.len(), 1);
    }

    #[test]
    fn resolve_with_include() {
        // project inherits ubuntu-24.04 and includes claude mixin.
        let project = Config {
            base: Some(BaseConfig {
                from: Some("ubuntu-24.04".to_string()),
                ..Default::default()
            }),
            vm: Some(VmConfig {
                memory: Some("16G".to_string()),
                cpus: None,
                disk: None,
                user: None,
            }),
            include: vec!["devtools".to_string(), "claude".to_string()],
            files: vec![],
            setup: vec![],
            provision: vec![ProvisionStep {
                source: None,
                run: Some("echo project".to_string()),
                script: None,
            }],
        };

        let resolved = resolve(project).unwrap();
        assert_eq!(resolved.memory, "16G"); // from project
        assert_eq!(resolved.cpus, 2); // from ubuntu default
        assert_eq!(resolved.disk, "20G"); // from ubuntu
        assert_eq!(resolved.user, "agent"); // from ubuntu

        // devtools mixin has 1 setup step.
        assert_eq!(resolved.setup.len(), 1);
        // claude mixin has 1 provision step, project adds 1 more.
        assert_eq!(resolved.provision.len(), 2);
        // Include steps come before project steps.
        assert!(resolved.provision[0]
            .run
            .as_deref()
            .unwrap()
            .contains("claude.ai"));
        assert_eq!(resolved.provision[1].run.as_deref(), Some("echo project"));
    }

    #[test]
    fn resolve_missing_image_errors() {
        let config = Config {
            base: Some(BaseConfig {
                from: Some("nonexistent-image".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let result = resolve(config);
        assert!(result.is_err());
        let err = format!("{:#}", result.unwrap_err());
        assert!(
            err.contains("not found"),
            "expected 'not found' error, got: {err}"
        );
    }

    #[test]
    fn merge_scalars_child_wins() {
        let parent = ResolvedConfig {
            base_url: "https://example.com/base.img".to_string(),
            base_checksum: "sha256:abc".to_string(),
            skip_checksum: false,
            memory: "2G".to_string(),
            cpus: 2,
            disk: "20G".to_string(),
            user: "agent".to_string(),
            files: vec![],
            setup: vec![],
            provision: vec![],
            template_name: None,
        };

        let child = Config {
            base: None,
            vm: Some(VmConfig {
                memory: Some("8G".to_string()),
                cpus: Some(4),
                disk: None,
                user: None,
            }),
            include: vec![],
            files: vec![],
            setup: vec![],
            provision: vec![],
        };

        let result = merge(parent, child);
        assert_eq!(result.memory, "8G");
        assert_eq!(result.cpus, 4);
        assert_eq!(result.disk, "20G"); // parent
        assert_eq!(result.user, "agent"); // parent
        assert_eq!(result.base_url, "https://example.com/base.img");
    }

    #[test]
    fn merge_lists_accumulate() {
        let parent = ResolvedConfig {
            base_url: "https://example.com/base.img".to_string(),
            base_checksum: "sha256:abc".to_string(),
            skip_checksum: false,
            memory: "2G".to_string(),
            cpus: 2,
            disk: "20G".to_string(),
            user: "agent".to_string(),
            files: vec![FileEntry {
                source: "parent-src".to_string(),
                dest: "parent-dst".to_string(),
            }],
            setup: vec![ProvisionStep {
                source: None,
                run: Some("echo parent-setup".to_string()),
                script: None,
            }],
            provision: vec![ProvisionStep {
                source: None,
                run: Some("echo parent".to_string()),
                script: None,
            }],
            template_name: None,
        };

        let child = Config {
            base: None,
            vm: None,
            include: vec![],
            files: vec![FileEntry {
                source: "child-src".to_string(),
                dest: "child-dst".to_string(),
            }],
            setup: vec![ProvisionStep {
                source: None,
                run: Some("echo child-setup".to_string()),
                script: None,
            }],
            provision: vec![ProvisionStep {
                source: None,
                run: Some("echo child".to_string()),
                script: None,
            }],
        };

        let result = merge(parent, child);
        assert_eq!(result.files.len(), 2);
        assert_eq!(result.files[0].source, "parent-src");
        assert_eq!(result.files[1].source, "child-src");
        assert_eq!(result.setup.len(), 2);
        assert_eq!(
            result.setup[0].run.as_deref(),
            Some("echo parent-setup")
        );
        assert_eq!(
            result.setup[1].run.as_deref(),
            Some("echo child-setup")
        );
        assert_eq!(result.provision.len(), 2);
        assert_eq!(result.provision[0].run.as_deref(), Some("echo parent"));
        assert_eq!(result.provision[1].run.as_deref(), Some("echo child"));
    }

    #[test]
    fn build_from_cli_minimal() {
        let args = minimal_args();
        let resolved = build_from_cli(&args).unwrap();
        // Should resolve to ubuntu-24.04 defaults.
        assert_eq!(resolved.memory, "2G");
        assert_eq!(resolved.cpus, 2);
        assert_eq!(resolved.disk, "20G");
        assert_eq!(resolved.user, "agent");
        assert!(!resolved.base_url.is_empty());
    }

    #[test]
    fn build_from_cli_image_flag() {
        let args = CreateArgs {
            image: Some("ubuntu-24.04".to_string()),
            memory: Some("8G".to_string()),
            ..minimal_args()
        };
        let resolved = build_from_cli(&args).unwrap();
        assert_eq!(resolved.cpus, 2); // ubuntu defaults
        assert_eq!(resolved.memory, "8G"); // CLI override
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
        let resolved = build_from_cli(&args).unwrap();
        assert_eq!(resolved.files.len(), 2);
        assert_eq!(resolved.files[0].source, "./setup.sh");
        assert_eq!(resolved.files[0].dest, "/home/agent/setup.sh");
        assert_eq!(resolved.files[1].source, "/etc/hosts");
        assert_eq!(resolved.files[1].dest, "/etc/hosts");
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
        assert!(
            err.contains("invalid --file format"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn build_from_cli_provisions() {
        let args = CreateArgs {
            provisions: vec!["apt-get update".to_string()],
            provision_scripts: vec!["./setup.sh".to_string()],
            ..minimal_args()
        };
        let resolved = build_from_cli(&args).unwrap();
        assert_eq!(resolved.provision.len(), 2);
        assert_eq!(
            resolved.provision[0].run.as_deref(),
            Some("apt-get update")
        );
        assert!(resolved.provision[0].script.is_none());
        assert!(resolved.provision[1].run.is_none());
        assert_eq!(
            resolved.provision[1].script.as_deref(),
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
[base]
from = "ubuntu-24.04"

[vm]
memory = "8G"
cpus = 4
"#,
        )
        .unwrap();

        let args = CreateArgs {
            config: Some(config_path.to_str().unwrap().to_string()),
            ..minimal_args()
        };
        let resolved = build_from_cli(&args).unwrap();
        assert_eq!(resolved.memory, "8G");
        assert_eq!(resolved.cpus, 4);
    }

    #[test]
    fn build_from_cli_cli_overrides_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("agv.toml");
        std::fs::write(
            &config_path,
            r#"
[base]
from = "ubuntu-24.04"

[vm]
memory = "8G"
cpus = 4
"#,
        )
        .unwrap();

        let args = CreateArgs {
            config: Some(config_path.to_str().unwrap().to_string()),
            memory: Some("16G".to_string()),
            cpus: None, // should keep config value
            ..minimal_args()
        };
        let resolved = build_from_cli(&args).unwrap();
        assert_eq!(resolved.memory, "16G");
        assert_eq!(resolved.cpus, 4); // kept from config
    }

    #[tokio::test]
    async fn save_and_reload_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let config = ResolvedConfig {
            base_url: "https://example.com/base.img".to_string(),
            base_checksum: "sha256:abc".to_string(),
            skip_checksum: false,
            memory: "4G".to_string(),
            cpus: 8,
            disk: "50G".to_string(),
            user: "testuser".to_string(),
            files: vec![FileEntry {
                source: "/tmp/src".to_string(),
                dest: "/home/agent/dst".to_string(),
            }],
            setup: vec![],
            provision: vec![ProvisionStep {
                source: None,
                run: Some("echo hello".to_string()),
                script: None,
            }],
            template_name: None,
        };

        save(&config, &path).await.unwrap();
        let reloaded = load_resolved(&path).unwrap();

        assert_eq!(reloaded.base_url, "https://example.com/base.img");
        assert_eq!(reloaded.memory, "4G");
        assert_eq!(reloaded.cpus, 8);
        assert_eq!(reloaded.disk, "50G");
        assert_eq!(reloaded.user, "testuser");

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
