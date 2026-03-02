//! `agv init` — write a starter agv.toml to the current directory.

use anyhow::{bail, Context as _};

const TEMPLATES: &[(&str, &str)] = &[
    ("claude",   include_str!("../examples/claude/agv.toml")),
    ("gemini",   include_str!("../examples/gemini/agv.toml")),
    ("codex",    include_str!("../examples/codex/agv.toml")),
    ("openclaw", include_str!("../examples/openclaw/agv.toml")),
];

const DEFAULT_CONTENT: &str = r#"# agv VM configuration.
# Run `agv images` to see available base images and mixins.
# Run `agv specs` to see available hardware sizes.
# Templates: agv init claude | gemini | codex | openclaw

[base]
from = "ubuntu-24.04"
# include = ["devtools"]           # git, curl, build-essential
# include = ["devtools", "claude"] # + Claude Code AI agent
spec = "medium"  # 2G RAM, 2 vCPUs, 20G disk

# Override individual resource settings if needed:
# [vm]
# memory = "8G"
# cpus = 4
# disk = "40G"

# Copy files into the VM:
# [[files]]
# source = "~/.gitconfig"
# dest   = "~/.gitconfig"

# Run as root during OS setup:
# [[setup]]
# run = "apt-get install -y <package>"

# Run as your user after setup:
# [[provision]]
# run = "git clone git@github.com:org/repo.git ~/repo"
"#;

pub fn run(template: Option<&str>, force: bool) -> anyhow::Result<()> {
    run_to(std::path::Path::new("agv.toml"), template, force)
}

fn run_to(dest: &std::path::Path, template: Option<&str>, force: bool) -> anyhow::Result<()> {
    if dest.exists() && !force {
        bail!("agv.toml already exists. Use --force to overwrite.");
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

    std::fs::write(dest, content).context("failed to write agv.toml")?;

    let label = template.unwrap_or("default");
    println!("  Wrote agv.toml ({label})");
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
    fn run_template_claude() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("agv.toml");
        run_to(&dest, Some("claude"), false).unwrap();
        let content = std::fs::read_to_string(&dest).unwrap();
        assert!(content.contains("claude"));
        assert!(content.contains("devtools"));
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
}
