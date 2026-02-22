//! Template variable expansion for config values.
//!
//! Supports `{{VAR}}` (required) and `{{VAR:-default}}` (with fallback)
//! syntax. Variables are sourced from host environment variables and an
//! optional `.env` file in the current directory.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context as _};

use crate::config::ResolvedConfig;

/// Parse a `.env` file into key-value pairs.
///
/// Handles `KEY=VALUE` lines, ignoring comments (`#`) and blank lines.
/// Values may be optionally quoted with single or double quotes.
pub fn load_dotenv(path: &Path) -> anyhow::Result<HashMap<String, String>> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read .env file {}", path.display()))?;

    let mut vars = HashMap::new();
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };

        let key = key.trim().to_string();
        let mut value = value.trim().to_string();

        // Strip matching quotes.
        if (value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\''))
        {
            value = value[1..value.len() - 1].to_string();
        }

        vars.insert(key, value);
    }

    Ok(vars)
}

/// Load template variables from `.env` (if present) and host environment.
///
/// Host environment variables take priority over `.env` values.
#[must_use]
pub fn load_variables() -> HashMap<String, String> {
    let mut vars = HashMap::new();

    // Load .env first (lower priority).
    let dotenv_path = Path::new(".env");
    if dotenv_path.exists() {
        if let Ok(dotenv_vars) = load_dotenv(dotenv_path) {
            vars.extend(dotenv_vars);
        }
    }

    // Host env vars override .env.
    for (key, value) in std::env::vars() {
        vars.insert(key, value);
    }

    vars
}

/// Expand `{{VAR}}` and `{{VAR:-default}}` placeholders in a string.
///
/// - `{{VAR}}` — required, fails if `VAR` is not in `vars`.
/// - `{{VAR:-default}}` — uses `default` if `VAR` is missing.
#[allow(clippy::implicit_hasher)]
pub fn expand(input: &str, vars: &HashMap<String, String>) -> anyhow::Result<String> {
    let mut result = String::with_capacity(input.len());
    let mut remaining = input;

    while let Some(start) = remaining.find("{{") {
        result.push_str(&remaining[..start]);

        let after_open = &remaining[start + 2..];
        let Some(end) = after_open.find("}}") else {
            bail!("unclosed template variable in: {input}");
        };

        let expr = &after_open[..end];

        // Check for default value syntax: VAR:-default
        let value = if let Some((var_name, default)) = expr.split_once(":-") {
            let var_name = var_name.trim();
            vars.get(var_name)
                .cloned()
                .unwrap_or_else(|| default.to_string())
        } else {
            let var_name = expr.trim();
            vars.get(var_name).cloned().ok_or_else(|| {
                anyhow::anyhow!("template variable '{var_name}' is not set and has no default")
            })?
        };

        result.push_str(&value);
        remaining = &after_open[end + 2..];
    }

    result.push_str(remaining);
    Ok(result)
}

/// Expand template variables in all expandable fields of a `ResolvedConfig`.
///
/// Expands: `ProvisionStep.run`, `ProvisionStep.script`, `FileEntry.source`,
/// `FileEntry.dest` across `files`, `setup`, and `provision` lists.
#[allow(clippy::implicit_hasher)]
pub fn expand_config(
    config: &mut ResolvedConfig,
    vars: &HashMap<String, String>,
) -> anyhow::Result<()> {
    // Expand file entries.
    for file in &mut config.files {
        file.source = expand(&file.source, vars).context("expanding file source path")?;
        file.dest = expand(&file.dest, vars).context("expanding file dest path")?;
    }

    // Expand setup steps.
    for step in &mut config.setup {
        if let Some(ref run) = step.run {
            step.run = Some(expand(run, vars).context("expanding setup run command")?);
        }
        if let Some(ref script) = step.script {
            step.script = Some(expand(script, vars).context("expanding setup script path")?);
        }
    }

    // Expand provision steps.
    for step in &mut config.provision {
        if let Some(ref run) = step.run {
            step.run = Some(expand(run, vars).context("expanding provision run command")?);
        }
        if let Some(ref script) = step.script {
            step.script =
                Some(expand(script, vars).context("expanding provision script path")?);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FileEntry, ProvisionStep};

    #[test]
    fn expand_simple_variable() {
        let mut vars = HashMap::new();
        vars.insert("NAME".to_string(), "world".to_string());
        assert_eq!(expand("hello {{NAME}}", &vars).unwrap(), "hello world");
    }

    #[test]
    fn expand_multiple_variables() {
        let mut vars = HashMap::new();
        vars.insert("A".to_string(), "1".to_string());
        vars.insert("B".to_string(), "2".to_string());
        assert_eq!(expand("{{A}} and {{B}}", &vars).unwrap(), "1 and 2");
    }

    #[test]
    fn expand_with_default() {
        let vars = HashMap::new();
        assert_eq!(
            expand("{{MISSING:-fallback}}", &vars).unwrap(),
            "fallback"
        );
    }

    #[test]
    fn expand_default_not_used_when_set() {
        let mut vars = HashMap::new();
        vars.insert("KEY".to_string(), "actual".to_string());
        assert_eq!(expand("{{KEY:-fallback}}", &vars).unwrap(), "actual");
    }

    #[test]
    fn expand_missing_required_fails() {
        let vars = HashMap::new();
        let result = expand("{{MISSING}}", &vars);
        assert!(result.is_err());
        let err = format!("{:#}", result.unwrap_err());
        assert!(err.contains("MISSING"), "error should name the variable: {err}");
    }

    #[test]
    fn expand_unclosed_brace_fails() {
        let vars = HashMap::new();
        let result = expand("{{UNCLOSED", &vars);
        assert!(result.is_err());
        let err = format!("{:#}", result.unwrap_err());
        assert!(err.contains("unclosed"), "error: {err}");
    }

    #[test]
    fn expand_no_placeholders() {
        let vars = HashMap::new();
        assert_eq!(
            expand("no variables here", &vars).unwrap(),
            "no variables here"
        );
    }

    #[test]
    fn expand_empty_default() {
        let vars = HashMap::new();
        assert_eq!(expand("pre{{VAR:-}}post", &vars).unwrap(), "prepost");
    }

    #[test]
    fn expand_whitespace_in_var_name() {
        let mut vars = HashMap::new();
        vars.insert("VAR".to_string(), "val".to_string());
        assert_eq!(expand("{{ VAR }}", &vars).unwrap(), "val");
    }

    #[test]
    fn load_dotenv_basic() {
        let dir = tempfile::tempdir().unwrap();
        let env_path = dir.path().join(".env");
        std::fs::write(
            &env_path,
            r#"
# A comment
KEY1=value1
KEY2="quoted value"
KEY3='single quoted'
EMPTY=

  SPACED = spaced_value
"#,
        )
        .unwrap();

        let vars = load_dotenv(&env_path).unwrap();
        assert_eq!(vars.get("KEY1").unwrap(), "value1");
        assert_eq!(vars.get("KEY2").unwrap(), "quoted value");
        assert_eq!(vars.get("KEY3").unwrap(), "single quoted");
        assert_eq!(vars.get("EMPTY").unwrap(), "");
        assert_eq!(vars.get("SPACED").unwrap(), "spaced_value");
    }

    #[test]
    fn expand_config_expands_all_fields() {
        let mut vars = HashMap::new();
        vars.insert("HOME".to_string(), "/home/user".to_string());
        vars.insert("API_KEY".to_string(), "sk-123".to_string());

        let mut config = ResolvedConfig {
            base_url: "https://example.com/img.qcow2".to_string(),
            base_checksum: "sha256:abc".to_string(),
            skip_checksum: false,
            memory: "2G".to_string(),
            cpus: 2,
            disk: "20G".to_string(),
            user: "agent".to_string(),
            files: vec![FileEntry {
                source: "{{HOME}}/.ssh/id_ed25519".to_string(),
                dest: "/home/agent/.ssh/id_ed25519".to_string(),
            }],
            setup: vec![ProvisionStep {
                source: None,
                run: Some("echo {{API_KEY}}".to_string()),
                script: None,
            }],
            provision: vec![
                ProvisionStep {
                    source: None,
                    run: Some("export KEY={{API_KEY}}".to_string()),
                    script: None,
                },
                ProvisionStep {
                    source: None,
                    run: None,
                    script: Some("{{HOME}}/scripts/setup.sh".to_string()),
                },
            ],
        };

        expand_config(&mut config, &vars).unwrap();

        assert_eq!(config.files[0].source, "/home/user/.ssh/id_ed25519");
        assert_eq!(config.setup[0].run.as_deref(), Some("echo sk-123"));
        assert_eq!(
            config.provision[0].run.as_deref(),
            Some("export KEY=sk-123")
        );
        assert_eq!(
            config.provision[1].script.as_deref(),
            Some("/home/user/scripts/setup.sh")
        );
    }
}
