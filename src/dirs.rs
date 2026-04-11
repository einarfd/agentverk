//! Platform-specific state and cache directory paths.
//!
//! On macOS: `~/Library/Application Support/agv/`
//! On Linux: `~/.local/share/agv/`
//!
//! The data directory can be overridden via [`set_data_dir`] for testing.

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

/// Return the directory for user-provided image definitions.
pub fn images_dir() -> anyhow::Result<PathBuf> {
    Ok(data_dir()?.join("images"))
}

/// Return the directory where VM templates are stored.
pub fn templates_dir() -> anyhow::Result<PathBuf> {
    Ok(data_dir()?.join("templates"))
}

/// Return the directory where user-provided spec definitions are stored.
pub fn specs_dir() -> anyhow::Result<PathBuf> {
    Ok(data_dir()?.join("specs"))
}

/// Return the state directory for a specific VM instance.
pub fn instance_dir(name: &str) -> anyhow::Result<PathBuf> {
    Ok(instances_dir()?.join(name))
}

fn home_dir() -> anyhow::Result<PathBuf> {
    std::env::home_dir().context("could not determine home directory")
}

/// Ensure the core directory structure exists.
pub async fn ensure_dirs() -> anyhow::Result<()> {
    let dirs = [image_cache_dir()?, instances_dir()?, images_dir()?, templates_dir()?, specs_dir()?];
    for dir in &dirs {
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("failed to create directory {}", dir.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ends_with_components(path: &std::path::Path, suffix: &str) -> bool {
        let suffix_path = std::path::Path::new(suffix);
        path.ends_with(suffix_path)
    }

    #[test]
    fn data_dir_ends_with_agv() {
        let dir = data_dir().unwrap();
        assert!(
            ends_with_components(&dir, "agv"),
            "data_dir should end with 'agv', got: {}",
            dir.display()
        );
    }

    #[test]
    fn image_cache_dir_ends_with_expected_path() {
        let dir = image_cache_dir().unwrap();
        assert!(
            ends_with_components(&dir, "cache/images"),
            "image_cache_dir should end with 'cache/images', got: {}",
            dir.display()
        );
    }

    #[test]
    fn instances_dir_ends_with_instances() {
        let dir = instances_dir().unwrap();
        assert!(
            ends_with_components(&dir, "instances"),
            "instances_dir should end with 'instances', got: {}",
            dir.display()
        );
    }

    #[test]
    fn images_dir_ends_with_images() {
        let dir = images_dir().unwrap();
        assert!(
            ends_with_components(&dir, "images"),
            "images_dir should end with 'images', got: {}",
            dir.display()
        );
    }

    #[test]
    fn templates_dir_ends_with_templates() {
        let dir = templates_dir().unwrap();
        assert!(
            ends_with_components(&dir, "templates"),
            "templates_dir should end with 'templates', got: {}",
            dir.display()
        );
    }

    #[test]
    fn specs_dir_ends_with_specs() {
        let dir = specs_dir().unwrap();
        assert!(
            ends_with_components(&dir, "specs"),
            "specs_dir should end with 'specs', got: {}",
            dir.display()
        );
    }

    #[test]
    fn instance_dir_appends_name() {
        let dir = instance_dir("myvm").unwrap();
        assert!(
            ends_with_components(&dir, "instances/myvm"),
            "instance_dir('myvm') should end with 'instances/myvm', got: {}",
            dir.display()
        );
    }

    #[test]
    fn all_dirs_are_under_data_dir() {
        let base = data_dir().unwrap();
        assert!(image_cache_dir().unwrap().starts_with(&base));
        assert!(instances_dir().unwrap().starts_with(&base));
        assert!(images_dir().unwrap().starts_with(&base));
        assert!(templates_dir().unwrap().starts_with(&base));
        assert!(specs_dir().unwrap().starts_with(&base));
    }
}
