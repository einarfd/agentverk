//! Host-facing imperative instructions that the *human* invoker needs to
//! follow after agv finishes provisioning a VM.
//!
//! These come from `manual_steps = [...]` in mixin TOMLs (per-mixin) or
//! at the top level of a user's `agv.toml` (VM-specific). The resolver
//! collects them into [`ResolvedConfig`] and this module renders them as
//! a printable block plus exposes a helper for the call sites that want
//! to print directly.
//!
//! Audience is strictly the human invoker: agents inside the VM never
//! see this content, and shouldn't — these are tasks only a human can
//! complete (e.g. browser-based auth flows).
//!
//! Display points:
//!   1. End of first successful provision (`agv create --start` or the
//!      first `agv start`) — printed by [`crate::vm::provision::run_first_boot`].
//!   2. `agv inspect <vm>` — printed alongside other VM details.

use std::fmt::Write as _;

use anstyle::{AnsiColor, Style};

use crate::config::ResolvedConfig;

const CYAN: Style = AnsiColor::Cyan.on_default();
const BOLD: Style = Style::new().bold();

/// Render manual steps from the resolved config as a printable block.
///
/// Returns `None` when no manual steps are declared (so callers can omit
/// the section entirely instead of printing an empty header).
///
/// The block uses ANSI styling that anstream strips when stdout isn't a
/// TTY, so CI logs and piped output stay clean.
#[must_use]
pub fn render(config: &ResolvedConfig) -> Option<String> {
    if config.config_manual_steps.is_empty() && config.mixin_manual_steps.is_empty() {
        return None;
    }

    let mut out = String::new();
    let _ = writeln!(out, "{CYAN}{BOLD}Manual setup required{BOLD:#}{CYAN:#}");
    out.push('\n');

    // VM-specific steps from the user's own config first — they're often
    // the most relevant context for what to do.
    for step in &config.config_manual_steps {
        let _ = writeln!(out, "  - {step}");
    }

    // Then per-mixin steps, with the mixin name prefixed in bold so the
    // human can map each line back to "which mixin asked for this".
    for entry in &config.mixin_manual_steps {
        for step in &entry.steps {
            let _ = writeln!(out, "  - {BOLD}{}{BOLD:#}: {step}", entry.name);
        }
    }

    Some(out)
}

/// Print manual steps to the host's stdout via `anstream` (`NO_COLOR` / TTY
/// detection respected). No-op when no steps are declared.
pub fn print_to_host(config: &ResolvedConfig) {
    if let Some(rendered) = render(config) {
        anstream::println!();
        anstream::print!("{rendered}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MixinManualSteps;
    use std::collections::BTreeMap;

    fn empty() -> ResolvedConfig {
        ResolvedConfig {
            base_url: String::new(),
            base_checksum: String::new(),
            skip_checksum: false,
            memory: "2G".to_string(),
            cpus: 2,
            disk: "20G".to_string(),
            user: "agent".to_string(),
            os_family: "debian".to_string(),
            files: vec![],
            setup: vec![],
            provision: vec![],
            forwards: vec![],
            auto_forwards: BTreeMap::new(),
            template_name: None,
            mixins_applied: vec![],
            mixin_notes: vec![],
            config_notes: vec![],
            mixin_manual_steps: vec![],
            config_manual_steps: vec![],
        }
    }

    #[test]
    fn render_returns_none_when_no_steps() {
        assert!(render(&empty()).is_none());
    }

    #[test]
    fn render_lists_mixin_steps_with_attribution() {
        let mut cfg = empty();
        cfg.mixin_manual_steps = vec![MixinManualSteps {
            name: "claude".into(),
            steps: vec!["Run `claude login` inside the VM.".into()],
        }];
        let block = render(&cfg).expect("should produce a block");
        assert!(block.contains("Manual setup required"));
        assert!(block.contains("claude"));
        assert!(block.contains("Run `claude login` inside the VM."));
    }

    #[test]
    fn render_lists_config_steps_above_mixin_steps() {
        let mut cfg = empty();
        cfg.config_manual_steps = vec!["Configure VPN before starting work.".into()];
        cfg.mixin_manual_steps = vec![MixinManualSteps {
            name: "gh".into(),
            steps: vec!["Run `gh auth login`.".into()],
        }];
        let block = render(&cfg).expect("should produce a block");
        let vpn_pos = block.find("Configure VPN").unwrap();
        let gh_pos = block.find("gh auth login").unwrap();
        assert!(vpn_pos < gh_pos, "config-level steps must appear before mixin steps");
    }

    #[test]
    fn render_emits_one_bullet_per_step() {
        let mut cfg = empty();
        cfg.mixin_manual_steps = vec![MixinManualSteps {
            name: "openclaw".into(),
            steps: vec!["First action.".into(), "Second action.".into()],
        }];
        let block = render(&cfg).expect("should produce a block");
        assert_eq!(block.matches("openclaw").count(), 2);
        assert!(block.contains("First action."));
        assert!(block.contains("Second action."));
    }
}
