//! `agv init` — write a starter agv.toml.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _};

const TEMPLATES: &[(&str, &str)] = &[
    ("claude",   include_str!("../examples/claude/agv.toml")),
    ("gemini",   include_str!("../examples/gemini/agv.toml")),
    ("codex",    include_str!("../examples/codex/agv.toml")),
    ("openclaw", include_str!("../examples/openclaw/agv.toml")),
];

const DEFAULT_CONTENT: &str = r#"# agv VM configuration.
#
# Quick reference:
#   agv images   — list available base images and mixins
#   agv specs    — list available hardware sizes (small, medium, large, xlarge)
#   agv create --start <name>   — create and start this VM
#
# Ready-made configs for popular agents live in the examples/ directory
# of the agv repo (examples/claude, examples/gemini, examples/codex,
# examples/openclaw, examples/repo-checkout). You can also generate one
# directly: agv init claude, agv init gemini, etc.
#
# Full reference: docs/config.md

[base]
from    = "ubuntu-24.04"                # `agv images` to list more
# include = ["devtools"]                # git, curl, build-essential
# include = ["devtools", "claude"]      # + Claude Code (also: gemini, codex, openclaw)
spec    = "medium"                      # 2G RAM, 2 vCPUs, 20G disk (`agv specs` to see all)

# Override individual resource settings on top of the named spec:
# [vm]
# memory = "8G"
# cpus   = 4
# disk   = "40G"

# ── Files copied from the host into the VM ──────────────────────────────────
# Use {{HOME}} for host paths and /home/{{AGV_USER}} for VM paths.
# (~/ is NOT expanded — it would be passed literally to scp.)
#
# [[files]]
# source = "{{HOME}}/.gitconfig"
# dest   = "/home/{{AGV_USER}}/.gitconfig"

# ── Setup steps (run as root during OS setup) ───────────────────────────────
# [[setup]]
# run = "apt-get install -y ripgrep fd-find"

# ── Provision steps (run as your user after setup) ──────────────────────────
# [[provision]]
# run = "git clone git@github.com:org/repo.git ~/repo"

# ── Template variables and secrets ──────────────────────────────────────────
# Values support {{VAR}} and {{VAR:-default}} substitution. Put secrets
# in a .env file next to this one (add .env to .gitignore!) and reference
# them like:
#
#   [[provision]]
#   run = "gh auth login --with-token <<< '{{GITHUB_TOKEN}}'"
#
# See docs/repo-access.md for private-repo access patterns (PAT, SSH
# keys, deploy keys) and their security trade-offs.
"#;

pub fn run(template: Option<&str>, output: Option<&str>, force: bool) -> anyhow::Result<()> {
    let dest: PathBuf = output.map_or_else(|| PathBuf::from("agv.toml"), PathBuf::from);
    run_to(&dest, template, force)
}

fn run_to(dest: &Path, template: Option<&str>, force: bool) -> anyhow::Result<()> {
    if dest.exists() && !force {
        bail!(
            "{} already exists. Use --force to overwrite.",
            dest.display()
        );
    }

    let content: &str = match template {
        None => DEFAULT_CONTENT,
        Some(name) => {
            if let Some((_, c)) = TEMPLATES.iter().find(|(t, _)| *t == name) {
                c
            } else {
                let names: Vec<&str> = TEMPLATES.iter().map(|(n, _)| *n).collect();
                bail!("unknown template '{name}'. Available: {}", names.join(", "));
            }
        }
    };

    // Create parent directory if needed.
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create parent directory {}", parent.display())
            })?;
        }
    }

    std::fs::write(dest, content)
        .with_context(|| format!("failed to write {}", dest.display()))?;

    let label = template.unwrap_or("default");
    println!("  Wrote {} ({label})", dest.display());
    println!("  Run: agv create --start <name>");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_default_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("agv.toml");
        run_to(&dest, None, false).unwrap();
        assert!(dest.exists());
        let content = std::fs::read_to_string(&dest).unwrap();
        assert!(content.contains("ubuntu-24.04"));
    }

    #[test]
    fn default_content_uses_correct_template_syntax() {
        // Regression test: the default must not suggest ~/ in [[files]],
        // which is not expanded and won't work.
        assert!(DEFAULT_CONTENT.contains("{{HOME}}"));
        assert!(DEFAULT_CONTENT.contains("{{AGV_USER}}"));
        // The literal "~/.gitconfig" pattern (uncommented or in an example)
        // would be wrong — but ~/repo in a provision step is shell-expanded
        // inside the VM and is fine. Check the files example specifically:
        let files_section = DEFAULT_CONTENT
            .split("[[files]]")
            .nth(1)
            .expect("default should have a [[files]] example");
        let next_section_idx = files_section
            .find("[[")
            .or_else(|| files_section.find("──"))
            .unwrap_or(files_section.len());
        let files_section = &files_section[..next_section_idx];
        assert!(
            !files_section.contains("~/"),
            "the [[files]] example must not use ~/ (which is not expanded)"
        );
    }

    #[test]
    fn default_content_points_to_examples() {
        assert!(DEFAULT_CONTENT.contains("examples/"));
        assert!(DEFAULT_CONTENT.contains("docs/config.md"));
    }

    #[test]
    fn run_template_claude() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("agv.toml");
        run_to(&dest, Some("claude"), false).unwrap();
        let content = std::fs::read_to_string(&dest).unwrap();
        assert!(content.contains("claude"));
        assert!(content.contains("devtools"));
    }

    #[test]
    fn run_writes_to_custom_output_path() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("subdir").join("my-config.toml");
        run_to(&dest, None, false).unwrap();
        assert!(dest.exists(), "file should exist at custom path");
    }

    #[test]
    fn run_fails_if_exists_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("agv.toml");
        run_to(&dest, None, false).unwrap();
        let result = run_to(&dest, None, false);
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("already exists"));
        // Error message should include the actual path, not a hardcoded
        // "agv.toml".
        assert!(
            msg.contains(dest.to_str().unwrap()),
            "error message should include the actual path, got: {msg}"
        );
    }

    #[test]
    fn run_force_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("agv.toml");
        run_to(&dest, None, false).unwrap();
        run_to(&dest, Some("claude"), true).unwrap();
        let content = std::fs::read_to_string(&dest).unwrap();
        assert!(content.contains("claude"));
    }

    #[test]
    fn run_unknown_template_errors() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("agv.toml");
        let result = run_to(&dest, Some("bogus"), false);
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("unknown template"));
        assert!(msg.contains("Available"));
    }

    #[test]
    fn all_templates_parse_as_valid_toml() {
        for (name, content) in TEMPLATES {
            let parsed: Result<toml::Value, _> = toml::from_str(content);
            assert!(parsed.is_ok(), "template '{name}' is not valid TOML");
        }
    }

    #[test]
    fn default_content_is_valid_toml() {
        let parsed: Result<toml::Value, _> = toml::from_str(DEFAULT_CONTENT);
        assert!(parsed.is_ok(), "default content is not valid TOML: {parsed:?}");
    }
}
