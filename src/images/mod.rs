//! Built-in and user image registry.
//!
//! Image definitions are TOML files that describe how to set up a VM.
//! Built-in images are embedded in the binary via `include_str!()`.
//! Users can add custom images in `<data_dir>/images/*.toml`.

use std::path::PathBuf;

use anyhow::Context as _;
use serde::Serialize;

use crate::config::Config;
use crate::dirs;

const UBUNTU_TOML: &str = include_str!("ubuntu-24.04.toml");
const DEBIAN_12_TOML: &str = include_str!("debian-12.toml");
const FEDORA_43_TOML: &str = include_str!("fedora-43.toml");
const CLAUDE_TOML: &str = include_str!("claude.toml");
const CODEX_TOML: &str = include_str!("codex.toml");
const DEVTOOLS_TOML: &str = include_str!("devtools.toml");
const DOCKER_TOML: &str = include_str!("docker.toml");
const GUI_XFCE_TOML: &str = include_str!("gui-xfce.toml");
const GEMINI_TOML: &str = include_str!("gemini.toml");
const GH_TOML: &str = include_str!("gh.toml");
const NODEJS_TOML: &str = include_str!("nodejs.toml");
const OPENCLAW_TOML: &str = include_str!("openclaw.toml");
const RUST_TOML: &str = include_str!("rust.toml");
const UV_TOML: &str = include_str!("uv.toml");
const ZSH_TOML: &str = include_str!("zsh.toml");
const OH_MY_ZSH_TOML: &str = include_str!("oh-my-zsh.toml");
const SSH_KEY_TOML: &str = include_str!("ssh-key.toml");

const BUILTIN_IMAGES: &[(&str, &str)] = &[
    ("ubuntu-24.04", UBUNTU_TOML),
    ("debian-12", DEBIAN_12_TOML),
    ("fedora-43", FEDORA_43_TOML),
    ("claude", CLAUDE_TOML),
    ("codex", CODEX_TOML),
    ("devtools", DEVTOOLS_TOML),
    ("docker", DOCKER_TOML),
    ("gui-xfce", GUI_XFCE_TOML),
    ("gemini", GEMINI_TOML),
    ("gh", GH_TOML),
    ("nodejs", NODEJS_TOML),
    ("oh-my-zsh", OH_MY_ZSH_TOML),
    ("openclaw", OPENCLAW_TOML),
    ("rust", RUST_TOML),
    ("ssh-key", SSH_KEY_TOML),
    ("uv", UV_TOML),
    ("zsh", ZSH_TOML),
];

/// Shorthand aliases for base images — purely CLI-time sugar so users can
/// type `--image ubuntu` instead of `--image ubuntu-24.04`.
///
/// Aliases resolve to the canonical name before lookup; they never appear
/// in saved instance configs, and `agv images` lists canonical names only.
/// When we bump a distro (e.g. Ubuntu 26.04), moving an alias is a
/// deliberate, documented change in the CHANGELOG — scripts that want
/// stability should pin to the canonical name.
const ALIASES: &[(&str, &str)] = &[
    ("ubuntu", "ubuntu-24.04"),
    ("debian", "debian-12"),
    ("fedora", "fedora-43"),
];

/// Resolve a possible alias to its canonical image name.
///
/// Returns the input unchanged if it isn't a known alias.
fn resolve_alias(name: &str) -> &str {
    ALIASES
        .iter()
        .find_map(|&(alias, canonical)| (alias == name).then_some(canonical))
        .unwrap_or(name)
}

/// Whether an image definition is a full image or a mixin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ImageType {
    /// A full image with a base image URL or parent reference.
    Image,
    /// A mixin that only contributes files, setup, and/or provision steps.
    Mixin,
}

impl std::fmt::Display for ImageType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Image => write!(f, "image"),
            Self::Mixin => write!(f, "mixin"),
        }
    }
}

/// Classify a config as either a full image or a mixin.
///
/// A config is an image if it has `base.from`, `base.aarch64`, `base.x86_64`,
/// or a `[vm]` section. Otherwise it's a mixin.
fn classify(config: &Config) -> ImageType {
    if let Some(ref base) = config.base {
        if base.from.is_some() || base.aarch64.is_some() || base.x86_64.is_some() {
            return ImageType::Image;
        }
    }
    if config.vm.is_some() {
        return ImageType::Image;
    }
    ImageType::Mixin
}

/// Information about an available image.
#[derive(Debug)]
pub struct ImageInfo {
    /// Image name (derived from filename or built-in key).
    pub name: String,
    /// Whether this is a full image or a mixin.
    pub image_type: ImageType,
    /// Where this image comes from.
    pub source: ImageSource,
}

/// JSON projection of `ImageInfo` for `agv images --json`.
///
/// Stable across the 0.x series — additions OK, removals/renames need
/// a major bump.
#[derive(Debug, Clone, Serialize)]
pub struct ImageJson {
    pub name: String,
    /// `"image"` or `"mixin"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// `true` for built-ins baked into the binary, `false` for
    /// user-provided files in `<data_dir>/images/`.
    pub built_in: bool,
    /// Path to the user-provided file, or `null` for built-ins.
    pub path: Option<String>,
}

impl From<&ImageInfo> for ImageJson {
    fn from(info: &ImageInfo) -> Self {
        let (built_in, path) = match &info.source {
            ImageSource::BuiltIn => (true, None),
            ImageSource::User(p) => (false, Some(p.display().to_string())),
        };
        Self {
            name: info.name.clone(),
            kind: info.image_type.to_string(),
            built_in,
            path,
        }
    }
}

/// Where an image definition was found.
#[derive(Debug)]
pub enum ImageSource {
    /// Baked into the binary.
    BuiltIn,
    /// User-provided file.
    User(PathBuf),
}

/// Look up an image definition by name.
///
/// Search order: user images dir → alias table → built-in images.
/// Aliases only apply to the built-in side — a user-provided file wins
/// over an alias even if its name matches one.
///
/// Returns `None` if no image with that name exists.
pub fn lookup(name: &str) -> anyhow::Result<Option<Config>> {
    // Check user images first — they override both built-ins and aliases.
    if let Ok(user_dir) = dirs::images_dir() {
        let user_path = user_dir.join(format!("{name}.toml"));
        if user_path.exists() {
            let contents = std::fs::read_to_string(&user_path)
                .with_context(|| format!("failed to read image file {}", user_path.display()))?;
            let config: Config = toml::from_str(&contents)
                .with_context(|| format!("failed to parse image file {}", user_path.display()))?;
            return Ok(Some(config));
        }
    }

    // Resolve any alias before consulting the built-in registry, so
    // `--image ubuntu` routes to the same config as `--image ubuntu-24.04`.
    let canonical = resolve_alias(name);

    // Check built-in images.
    for &(builtin_name, toml_str) in BUILTIN_IMAGES {
        if builtin_name == canonical {
            let config: Config = toml::from_str(toml_str)
                .with_context(|| format!("failed to parse built-in image '{canonical}'"))?;
            return Ok(Some(config));
        }
    }

    Ok(None)
}

/// List all available images (user images + built-in).
///
/// User images with the same name as a built-in image shadow the built-in.
pub fn list_all() -> anyhow::Result<Vec<ImageInfo>> {
    let mut images = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // User images first (they take precedence).
    if let Ok(user_dir) = dirs::images_dir() {
        if user_dir.exists() {
            let entries = std::fs::read_dir(&user_dir)
                .with_context(|| format!("failed to read images dir {}", user_dir.display()))?;
            for entry in entries {
                let entry = entry?;
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "toml") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        let name = stem.to_string();
                        seen.insert(name.clone());
                        let config: Config = toml::from_str(
                            &std::fs::read_to_string(&path).with_context(|| {
                                format!("failed to read image file {}", path.display())
                            })?,
                        )
                        .with_context(|| {
                            format!("failed to parse image file {}", path.display())
                        })?;
                        images.push(ImageInfo {
                            name,
                            image_type: classify(&config),
                            source: ImageSource::User(path),
                        });
                    }
                }
            }
        }
    }

    // Built-in images (skip if shadowed by user image).
    for &(name, toml_str) in BUILTIN_IMAGES {
        if !seen.contains(name) {
            let config: Config = toml::from_str(toml_str)
                .with_context(|| format!("failed to parse built-in image '{name}'"))?;
            images.push(ImageInfo {
                name: name.to_string(),
                image_type: classify(&config),
                source: ImageSource::BuiltIn,
            });
        }
    }

    // Sort: images before mixins, then alphabetically within each group.
    images.sort_by(|a, b| {
        a.image_type
            .cmp(&b.image_type)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(images)
}

impl std::fmt::Display for ImageSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BuiltIn => write!(f, "built-in"),
            Self::User(path) => write!(f, "{}", path.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_builtin_ubuntu() {
        let config = lookup("ubuntu-24.04").unwrap();
        assert!(config.is_some(), "ubuntu-24.04 should be a built-in image");

        let config = config.unwrap();
        let base = config.base.unwrap();
        assert!(base.from.is_none(), "ubuntu-24.04 is a root image");
        assert!(base.aarch64.is_some());
        assert!(base.x86_64.is_some());
    }

    #[test]
    fn lookup_builtin_debian_12() {
        let config = lookup("debian-12").unwrap();
        assert!(config.is_some(), "debian-12 should be a built-in image");

        let config = config.unwrap();
        let base = config.base.unwrap();
        assert!(base.from.is_none(), "debian-12 is a root image");
        assert!(base.aarch64.is_some());
        assert!(base.x86_64.is_some());
    }

    #[test]
    fn lookup_builtin_fedora_43() {
        let config = lookup("fedora-43").unwrap();
        assert!(config.is_some(), "fedora-43 should be a built-in image");

        let config = config.unwrap();
        let base = config.base.unwrap();
        assert!(base.from.is_none(), "fedora-43 is a root image");
        assert!(base.aarch64.is_some());
        assert!(base.x86_64.is_some());
        assert_eq!(base.os_family.as_deref(), Some("fedora"));
    }

    #[test]
    fn lookup_builtin_claude() {
        let config = lookup("claude").unwrap();
        assert!(config.is_some(), "claude should be a built-in image");

        let config = config.unwrap();
        assert!(config.base.is_none());
        assert!(config.vm.is_none());
        assert!(config.setup.is_empty(), "claude should have no setup steps");
        assert!(!config.provision.is_empty(), "claude should have provision steps");
    }

    #[test]
    fn lookup_builtin_devtools() {
        let config = lookup("devtools").unwrap();
        assert!(config.is_some(), "devtools should be a built-in image");

        let config = config.unwrap();
        assert!(config.base.is_none());
        assert!(config.vm.is_none());
        // devtools is per-family now — setup lives under [os_families.*].
        let families = config
            .os_families
            .as_ref()
            .expect("devtools should declare os_families sections");
        assert!(
            families.contains_key("debian"),
            "devtools should support debian"
        );
        assert!(
            families.contains_key("fedora"),
            "devtools should support fedora"
        );
    }

    #[test]
    fn lookup_builtin_docker() {
        let config = lookup("docker").unwrap();
        assert!(config.is_some(), "docker should be a built-in image");

        let config = config.unwrap();
        assert!(config.base.is_none());
        assert!(config.vm.is_none());
        assert!(!config.setup.is_empty(), "docker should have setup steps");
    }

    #[test]
    fn lookup_builtin_rust() {
        let config = lookup("rust").unwrap().unwrap();
        assert!(config.base.is_none());
        assert!(config.vm.is_none());
        // Top-level provision (rustup) is distro-agnostic; setup (build
        // deps) lives under [os_families.*].
        assert!(!config.provision.is_empty(), "rust should have provision steps");
        let families = config
            .os_families
            .as_ref()
            .expect("rust should declare os_families");
        assert!(families.contains_key("debian"));
        assert!(families.contains_key("fedora"));
    }

    #[test]
    fn lookup_builtin_uv() {
        let config = lookup("uv").unwrap();
        assert!(config.is_some(), "uv should be a built-in image");

        let config = config.unwrap();
        assert!(config.base.is_none());
        assert!(config.vm.is_none());
        assert!(!config.provision.is_empty(), "uv should have provision steps");
    }

    #[test]
    fn lookup_builtin_gh() {
        let config = lookup("gh").unwrap().unwrap();
        assert!(config.base.is_none());
        assert!(config.vm.is_none());
        let families = config
            .os_families
            .as_ref()
            .expect("gh should declare os_families");
        assert!(families.contains_key("debian"));
        assert!(families.contains_key("fedora"));
    }

    #[test]
    fn lookup_builtin_zsh() {
        let config = lookup("zsh").unwrap().unwrap();
        assert!(config.base.is_none());
        assert!(config.vm.is_none());
        let families = config
            .os_families
            .as_ref()
            .expect("zsh should declare os_families");
        assert!(families.contains_key("debian"));
        assert!(families.contains_key("fedora"));
    }

    #[test]
    fn lookup_builtin_oh_my_zsh() {
        let config = lookup("oh-my-zsh").unwrap().unwrap();
        // oh-my-zsh now depends on zsh via `include = ["zsh"]`.
        let base = config.base.expect("oh-my-zsh should have [base]");
        assert!(
            base.include.contains(&"zsh".to_string()),
            "oh-my-zsh should include zsh"
        );
        assert!(config.vm.is_none());
        // Its own provision step still installs oh-my-zsh.
        assert!(
            !config.provision.is_empty(),
            "oh-my-zsh should have its own provision step"
        );
    }

    #[test]
    fn lookup_builtin_nodejs() {
        let config = lookup("nodejs").unwrap().unwrap();
        assert!(config.base.is_none());
        assert!(config.vm.is_none());
        let families = config
            .os_families
            .as_ref()
            .expect("nodejs should declare os_families");
        assert!(families.contains_key("debian"));
        assert!(families.contains_key("fedora"));
    }

    #[test]
    fn lookup_builtin_gemini() {
        let config = lookup("gemini").unwrap().unwrap();
        assert!(config.base.is_none());
        assert!(config.vm.is_none());
        assert!(config.setup.is_empty(), "gemini should have no setup steps");
        assert!(!config.provision.is_empty(), "gemini should have provision steps");
    }

    #[test]
    fn lookup_builtin_openclaw() {
        let config = lookup("openclaw").unwrap().unwrap();
        assert!(config.base.is_none());
        assert!(config.vm.is_none());
        assert!(config.setup.is_empty(), "openclaw should have no setup steps");
        assert!(!config.provision.is_empty(), "openclaw should have provision steps");
    }

    #[test]
    fn lookup_builtin_codex() {
        let config = lookup("codex").unwrap().unwrap();
        assert!(config.base.is_none());
        assert!(config.vm.is_none());
        assert!(config.setup.is_empty(), "codex should have no setup steps");
        assert!(!config.provision.is_empty(), "codex should have provision steps");
    }

    #[test]
    fn lookup_nonexistent() {
        let config = lookup("does-not-exist-12345").unwrap();
        assert!(config.is_none());
    }

    #[test]
    fn alias_ubuntu_resolves_to_ubuntu_24_04() {
        let via_alias = lookup("ubuntu").unwrap().unwrap();
        let via_canonical = lookup("ubuntu-24.04").unwrap().unwrap();
        let base_alias = via_alias.base.unwrap();
        let base_canonical = via_canonical.base.unwrap();
        assert_eq!(base_alias.os_family, base_canonical.os_family);
        assert_eq!(
            base_alias.aarch64.as_ref().map(|a| a.url.clone()),
            base_canonical.aarch64.as_ref().map(|a| a.url.clone()),
        );
    }

    #[test]
    fn alias_debian_resolves_to_debian_12() {
        let via_alias = lookup("debian").unwrap().unwrap();
        let via_canonical = lookup("debian-12").unwrap().unwrap();
        assert_eq!(
            via_alias.base.as_ref().and_then(|b| b.os_family.clone()),
            via_canonical.base.as_ref().and_then(|b| b.os_family.clone()),
        );
    }

    #[test]
    fn alias_fedora_resolves_to_fedora_43() {
        let via_alias = lookup("fedora").unwrap().unwrap();
        let via_canonical = lookup("fedora-43").unwrap().unwrap();
        assert_eq!(
            via_alias.base.as_ref().and_then(|b| b.os_family.clone()),
            via_canonical.base.as_ref().and_then(|b| b.os_family.clone()),
        );
    }

    #[test]
    fn list_all_does_not_duplicate_aliases() {
        // Aliases are resolve-time sugar; they should not appear as
        // separate entries in `agv images` output.
        let images = list_all().unwrap();
        let names: Vec<&str> = images.iter().map(|i| i.name.as_str()).collect();
        for alias in ["ubuntu", "debian", "fedora"] {
            assert!(
                !names.contains(&alias),
                "alias '{alias}' should not appear in list_all output"
            );
        }
    }

    #[test]
    fn list_all_includes_builtins() {
        let images = list_all().unwrap();
        let names: Vec<&str> = images.iter().map(|i| i.name.as_str()).collect();
        assert!(names.contains(&"ubuntu-24.04"));
        assert!(names.contains(&"debian-12"));
        assert!(names.contains(&"fedora-43"));
        assert!(names.contains(&"claude"));
        assert!(names.contains(&"codex"));
        assert!(names.contains(&"devtools"));
        assert!(names.contains(&"docker"));
        assert!(names.contains(&"gemini"));
        assert!(names.contains(&"gh"));
        assert!(names.contains(&"nodejs"));
        assert!(names.contains(&"oh-my-zsh"));
        assert!(names.contains(&"openclaw"));
        assert!(names.contains(&"rust"));
        assert!(names.contains(&"ssh-key"));
        assert!(names.contains(&"uv"));
        assert!(names.contains(&"zsh"));
    }

    /// Schema pin for `agv images --json` entries — drift here is a
    /// major-version bump.
    #[test]
    fn image_json_schema_pin() {
        let info = ImageInfo {
            name: "claude".to_string(),
            image_type: ImageType::Mixin,
            source: ImageSource::BuiltIn,
        };
        let json = serde_json::to_value(ImageJson::from(&info)).unwrap();
        let obj = json.as_object().expect("ImageJson must serialize as an object");
        let actual: std::collections::BTreeSet<&str> =
            obj.keys().map(String::as_str).collect();
        let expected: std::collections::BTreeSet<&str> =
            ["built_in", "name", "path", "type"].into_iter().collect();
        assert_eq!(actual, expected, "ImageJson keys drifted");
        // Built-ins serialize path as null, not omit it.
        assert_eq!(obj.get("path"), Some(&serde_json::Value::Null));
        assert_eq!(
            obj.get("type"),
            Some(&serde_json::Value::String("mixin".to_string())),
        );
    }

    #[test]
    fn image_json_user_source_serializes_path() {
        let info = ImageInfo {
            name: "myimage".to_string(),
            image_type: ImageType::Image,
            source: ImageSource::User(PathBuf::from("/tmp/myimage.toml")),
        };
        let json = serde_json::to_value(ImageJson::from(&info)).unwrap();
        assert_eq!(json.get("built_in"), Some(&serde_json::Value::Bool(false)));
        assert_eq!(
            json.get("path"),
            Some(&serde_json::Value::String("/tmp/myimage.toml".to_string())),
        );
        assert_eq!(
            json.get("type"),
            Some(&serde_json::Value::String("image".to_string())),
        );
    }
}
