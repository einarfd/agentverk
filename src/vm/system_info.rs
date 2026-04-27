//! Composes `~/.agv/system.md` — a short, token-cheap summary of the VM's
//! shape for agents running inside it.
//!
//! Rendered once, at the end of first-boot provisioning, from the resolved
//! config. Agents can grep/read it to know the base OS, mixins in play, and
//! any non-obvious wiring a mixin declared via `notes = [...]`. The file
//! reserves a trailing section for agents to append their own install notes.
//!
//! Keep the output stable and short — this file is meant to end up inlined
//! (via Claude Code's `@~/.agv/system.md`, Gemini's equivalent, etc.) into
//! agent context every session, so every line is paid for in tokens.

use std::fmt::Write as _;

use crate::config::ResolvedConfig;

/// Render the contents of `~/.agv/system.md`.
///
/// `arch` is the guest architecture (`aarch64` / `x86_64`). The caller
/// passes it rather than reading `std::env::consts::ARCH` so hosts with
/// cross-arch VMs still render honestly.
#[must_use]
pub fn render(config: &ResolvedConfig, arch: &str) -> String {
    let mut out = String::new();

    out.push_str("# agv system info\n\n");
    out.push_str("_Initial VM state — what agv installed at first boot. May have drifted since._\n\n");
    writeln!(out, "- OS family: {} ({arch})", config.os_family).unwrap();
    writeln!(out, "- User: `{}` (passwordless sudo)", config.user).unwrap();

    // VM-specific notes from the user's own config (top-level `notes = [...]`).
    // Surfaced above the mixin list because they describe *this VM's* purpose,
    // not what individual mixins contribute.
    if !config.config_notes.is_empty() {
        out.push_str("\n## This VM\n\n");
        for note in &config.config_notes {
            writeln!(out, "- {note}").unwrap();
        }
    }

    if !config.mixins_applied.is_empty() {
        out.push_str("\n## Mixins\n\n");
        // Index mixin_notes by name so we can show every applied mixin in a
        // single pass, with or without its note. Mixins without notes still
        // appear (by name) so nothing is invisible to the agent.
        let notes_by_name: std::collections::HashMap<&str, &Vec<String>> =
            config
                .mixin_notes
                .iter()
                .map(|e| (e.name.as_str(), &e.notes))
                .collect();

        for name in &config.mixins_applied {
            match notes_by_name.get(name.as_str()) {
                Some(notes) if !notes.is_empty() => {
                    let mut iter = notes.iter();
                    // First note on the same line as the bold mixin name.
                    if let Some(first) = iter.next() {
                        writeln!(out, "- **{name}**: {first}").unwrap();
                    }
                    // Any additional notes render as continuation sub-bullets.
                    for note in iter {
                        writeln!(out, "  - {note}").unwrap();
                    }
                }
                _ => {
                    writeln!(out, "- **{name}**").unwrap();
                }
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MixinNotes;
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
            labels: BTreeMap::new(),
        }
    }

    #[test]
    fn renders_bare_minimum() {
        let md = render(&empty(), "aarch64");
        assert!(md.contains("# agv system info"));
        assert!(md.contains("Initial VM state"));
        assert!(md.contains("May have drifted"));
        assert!(md.contains("OS family: debian (aarch64)"));
        assert!(md.contains("User: `agent` (passwordless sudo)"));
        // No mixins section when nothing was applied.
        assert!(!md.contains("## Mixins"));
    }

    #[test]
    fn lists_mixins_with_and_without_notes() {
        let mut cfg = empty();
        cfg.mixins_applied = vec![
            "devtools".into(),
            "docker".into(),
            "plain-mixin".into(),
            "gui-xfce".into(),
        ];
        cfg.mixin_notes = vec![
            MixinNotes {
                name: "docker".into(),
                notes: vec!["service enabled; user in docker group".into()],
            },
            MixinNotes {
                name: "gui-xfce".into(),
                notes: vec![
                    "`agv gui <vm>` opens the XFCE desktop in your host browser".into(),
                ],
            },
        ];
        let md = render(&cfg, "x86_64");
        assert!(md.contains("## Mixins"));
        assert!(md.contains("- **docker**: service enabled"));
        assert!(md.contains("- **gui-xfce**: `agv gui <vm>`"));
        // Mixins without a declared note still appear — just the bare name.
        assert!(md.contains("- **devtools**\n"));
        assert!(md.contains("- **plain-mixin**\n"));
    }

    #[test]
    fn config_notes_render_in_their_own_section_above_mixins() {
        let mut cfg = empty();
        cfg.config_notes = vec![
            "This VM is for the foo project.".into(),
            "API key lives at {{HOME}}/.foo-secrets.".into(),
        ];
        cfg.mixins_applied = vec!["devtools".into()];
        let md = render(&cfg, "aarch64");
        assert!(md.contains("## This VM"));
        assert!(md.contains("- This VM is for the foo project."));
        assert!(md.contains("- API key lives at"));
        // Order: config notes section appears before the mixins section.
        let this_vm = md.find("## This VM").unwrap();
        let mixins = md.find("## Mixins").unwrap();
        assert!(this_vm < mixins, "## This VM must appear above ## Mixins");
    }

    #[test]
    fn no_config_notes_section_when_empty() {
        let md = render(&empty(), "aarch64");
        assert!(!md.contains("## This VM"));
    }

    #[test]
    fn multi_line_notes_render_as_sub_bullets() {
        let mut cfg = empty();
        cfg.mixins_applied = vec!["docker".into()];
        cfg.mixin_notes = vec![MixinNotes {
            name: "docker".into(),
            notes: vec!["first point".into(), "second point".into()],
        }];
        let md = render(&cfg, "aarch64");
        assert!(md.contains("- **docker**: first point"));
        assert!(md.contains("  - second point"));
    }

    #[test]
    fn renders_under_30_lines_for_typical_config() {
        // Agents will see this every session. Keep it cheap.
        let mut cfg = empty();
        cfg.mixins_applied = vec![
            "devtools".into(),
            "docker".into(),
            "gh".into(),
            "nodejs".into(),
            "rust".into(),
            "uv".into(),
            "zsh".into(),
            "oh-my-zsh".into(),
            "claude".into(),
            "gui-xfce".into(),
        ];
        cfg.mixin_notes = vec![MixinNotes {
            name: "docker".into(),
            notes: vec!["service enabled at boot".into()],
        }];
        let md = render(&cfg, "aarch64");
        assert!(
            md.lines().count() < 30,
            "rendered output is {} lines — trim it before this grows unchecked",
            md.lines().count()
        );
    }
}
