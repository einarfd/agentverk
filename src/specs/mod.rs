//! VM hardware specs — named presets for memory, CPUs, and disk.
//!
//! Built-in specs are embedded in the binary. Users can add custom specs in
//! `<data_dir>/specs/*.toml`, which take precedence over built-in ones.

use std::path::PathBuf;

use anyhow::Context as _;
use serde::Deserialize;

use crate::dirs;

const SMALL_TOML: &str = include_str!("small.toml");
const MEDIUM_TOML: &str = include_str!("medium.toml");
const LARGE_TOML: &str = include_str!("large.toml");
const XLARGE_TOML: &str = include_str!("xlarge.toml");

const BUILTIN_SPECS: &[(&str, &str)] = &[
    ("small", SMALL_TOML),
    ("medium", MEDIUM_TOML),
    ("large", LARGE_TOML),
    ("xlarge", XLARGE_TOML),
];

/// A VM hardware specification.
#[derive(Debug, Clone, Deserialize)]
pub struct Spec {
    /// Memory allocation, e.g. "4G".
    pub memory: String,
    /// Number of virtual CPUs.
    pub cpus: u32,
    /// Disk size, e.g. "20G".
    pub disk: String,
}

/// Information about an available spec.
#[derive(Debug)]
pub struct SpecInfo {
    /// Spec name.
    pub name: String,
    /// The parsed spec.
    pub spec: Spec,
    /// Where this spec comes from.
    pub source: SpecSource,
}

/// Where a spec definition was found.
#[derive(Debug)]
pub enum SpecSource {
    /// Baked into the binary.
    BuiltIn,
    /// User-provided file.
    User(PathBuf),
}

impl std::fmt::Display for SpecSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BuiltIn => write!(f, "built-in"),
            Self::User(path) => write!(f, "{}", path.display()),
        }
    }
}

/// Look up a spec by name.
///
/// Search order: user specs dir → built-in specs.
/// Returns `None` if no spec with that name exists.
pub fn lookup(name: &str) -> anyhow::Result<Option<Spec>> {
    // Check user specs first — they override built-in.
    if let Ok(user_dir) = dirs::specs_dir() {
        let user_path = user_dir.join(format!("{name}.toml"));
        if user_path.exists() {
            let contents = std::fs::read_to_string(&user_path)
                .with_context(|| format!("failed to read spec file {}", user_path.display()))?;
            let spec: Spec = toml::from_str(&contents)
                .with_context(|| format!("failed to parse spec file {}", user_path.display()))?;
            return Ok(Some(spec));
        }
    }

    // Check built-in specs.
    for &(builtin_name, toml_str) in BUILTIN_SPECS {
        if builtin_name == name {
            let spec: Spec = toml::from_str(toml_str)
                .with_context(|| format!("failed to parse built-in spec '{name}'"))?;
            return Ok(Some(spec));
        }
    }

    Ok(None)
}

/// List all available specs (user specs + built-in).
///
/// User specs with the same name as a built-in spec shadow the built-in.
/// Order matches the built-in order (small → medium → large → xlarge),
/// with user specs appended after.
pub fn list_all() -> anyhow::Result<Vec<SpecInfo>> {
    let mut specs = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // User specs first (they take precedence).
    if let Ok(user_dir) = dirs::specs_dir() {
        if user_dir.exists() {
            let entries = std::fs::read_dir(&user_dir)
                .with_context(|| format!("failed to read specs dir {}", user_dir.display()))?;
            for entry in entries {
                let entry = entry?;
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "toml") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        let name = stem.to_string();
                        seen.insert(name.clone());
                        let spec: Spec = toml::from_str(
                            &std::fs::read_to_string(&path).with_context(|| {
                                format!("failed to read spec file {}", path.display())
                            })?,
                        )
                        .with_context(|| {
                            format!("failed to parse spec file {}", path.display())
                        })?;
                        specs.push(SpecInfo {
                            name,
                            spec,
                            source: SpecSource::User(path),
                        });
                    }
                }
            }
        }
    }

    // Built-in specs (skip if shadowed by user spec).
    for &(name, toml_str) in BUILTIN_SPECS {
        if !seen.contains(name) {
            let spec: Spec = toml::from_str(toml_str)
                .with_context(|| format!("failed to parse built-in spec '{name}'"))?;
            specs.push(SpecInfo {
                name: name.to_string(),
                spec,
                source: SpecSource::BuiltIn,
            });
        }
    }

    Ok(specs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_builtin_small() {
        let spec = lookup("small").unwrap().unwrap();
        assert_eq!(spec.memory, "1G");
        assert_eq!(spec.cpus, 1);
        assert_eq!(spec.disk, "10G");
    }

    #[test]
    fn lookup_builtin_medium() {
        let spec = lookup("medium").unwrap().unwrap();
        assert_eq!(spec.memory, "2G");
        assert_eq!(spec.cpus, 2);
        assert_eq!(spec.disk, "20G");
    }

    #[test]
    fn lookup_builtin_large() {
        let spec = lookup("large").unwrap().unwrap();
        assert_eq!(spec.memory, "8G");
        assert_eq!(spec.cpus, 4);
        assert_eq!(spec.disk, "40G");
    }

    #[test]
    fn lookup_builtin_xlarge() {
        let spec = lookup("xlarge").unwrap().unwrap();
        assert_eq!(spec.memory, "16G");
        assert_eq!(spec.cpus, 8);
        assert_eq!(spec.disk, "80G");
    }

    #[test]
    fn lookup_nonexistent_returns_none() {
        assert!(lookup("nonexistent-12345").unwrap().is_none());
    }

    #[test]
    fn list_all_includes_all_builtins() {
        let specs = list_all().unwrap();
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"small"));
        assert!(names.contains(&"medium"));
        assert!(names.contains(&"large"));
        assert!(names.contains(&"xlarge"));
    }
}
