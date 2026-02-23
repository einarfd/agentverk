//! Built-in and user image registry.
//!
//! Image definitions are TOML files that describe how to set up a VM.
//! Built-in images are embedded in the binary via `include_str!()`.
//! Users can add custom images in `<data_dir>/images/*.toml`.

use std::path::PathBuf;

use anyhow::Context as _;

use crate::config::Config;
use crate::dirs;

const UBUNTU_TOML: &str = include_str!("ubuntu-24.04.toml");
const CLAUDE_TOML: &str = include_str!("claude.toml");
const DEVTOOLS_TOML: &str = include_str!("devtools.toml");
const DOCKER_TOML: &str = include_str!("docker.toml");
const GH_TOML: &str = include_str!("gh.toml");
const RUST_TOML: &str = include_str!("rust.toml");
const UV_TOML: &str = include_str!("uv.toml");
const ZSH_TOML: &str = include_str!("zsh.toml");

const BUILTIN_IMAGES: &[(&str, &str)] = &[
    ("ubuntu-24.04", UBUNTU_TOML),
    ("claude", CLAUDE_TOML),
    ("devtools", DEVTOOLS_TOML),
    ("docker", DOCKER_TOML),
    ("gh", GH_TOML),
    ("rust", RUST_TOML),
    ("uv", UV_TOML),
    ("zsh", ZSH_TOML),
];

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
/// Search order: user images dir → built-in images.
/// Returns `None` if no image with that name exists.
pub fn lookup(name: &str) -> anyhow::Result<Option<Config>> {
    // Check user images first — they override built-in.
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

    // Check built-in images.
    for &(builtin_name, toml_str) in BUILTIN_IMAGES {
        if builtin_name == name {
            let config: Config = toml::from_str(toml_str)
                .with_context(|| format!("failed to parse built-in image '{name}'"))?;
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
        assert!(!config.setup.is_empty(), "devtools should have setup steps");
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
        let config = lookup("rust").unwrap();
        assert!(config.is_some(), "rust should be a built-in image");

        let config = config.unwrap();
        assert!(config.base.is_none());
        assert!(config.vm.is_none());
        assert!(!config.setup.is_empty(), "rust should have setup steps");
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
        let config = lookup("gh").unwrap();
        assert!(config.is_some(), "gh should be a built-in image");
        let config = config.unwrap();
        assert!(config.base.is_none());
        assert!(config.vm.is_none());
        assert!(!config.setup.is_empty(), "gh should have setup steps");
    }

    #[test]
    fn lookup_builtin_zsh() {
        let config = lookup("zsh").unwrap();
        assert!(config.is_some(), "zsh should be a built-in image");
        let config = config.unwrap();
        assert!(config.base.is_none());
        assert!(config.vm.is_none());
        assert!(!config.setup.is_empty(), "zsh should have setup steps");
        assert!(!config.provision.is_empty(), "zsh should have provision steps");
    }

    #[test]
    fn lookup_nonexistent() {
        let config = lookup("does-not-exist-12345").unwrap();
        assert!(config.is_none());
    }

    #[test]
    fn list_all_includes_builtins() {
        let images = list_all().unwrap();
        let names: Vec<&str> = images.iter().map(|i| i.name.as_str()).collect();
        assert!(names.contains(&"ubuntu-24.04"));
        assert!(names.contains(&"claude"));
        assert!(names.contains(&"devtools"));
        assert!(names.contains(&"docker"));
        assert!(names.contains(&"gh"));
        assert!(names.contains(&"rust"));
        assert!(names.contains(&"uv"));
        assert!(names.contains(&"zsh"));
    }
}
