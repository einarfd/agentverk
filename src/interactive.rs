//! Interactive prompting for step-by-step provisioning.
//!
//! Used by `agv create --interactive` and `agv start --interactive` to let
//! users approve, skip, or edit each provisioning step as it runs.

use std::io::{BufRead, Write};

use anyhow::Context as _;

/// What to do with a single step.
#[derive(Debug, Clone)]
pub enum Decision {
    /// Run the step. The string is the (possibly edited) command to run.
    Run(String),
    /// Skip this step but continue with the next.
    Skip,
    /// Run the rest of the steps without further prompting.
    All(String),
    /// Abort the run.
    Quit,
}

/// Mutable state shared across step prompts in a single run.
#[derive(Debug, Default)]
pub struct InteractiveState {
    /// Set to true after the user picks "all" — subsequent steps run
    /// without prompting.
    pub all: bool,
}

impl InteractiveState {
    #[must_use]
    pub fn new() -> Self {
        Self { all: false }
    }
}

/// Prompt the user about a step. Returns what to do with it.
///
/// Shows the step's command and asks `[Y/n/e/a/q]`. With:
/// - `y` (default) — run the command as-is
/// - `n` — skip this step
/// - `e` — edit the command before running
/// - `a` — run this and all remaining steps without prompting
/// - `q` — quit (returns `Decision::Quit`, caller should bail)
///
/// `label` is a short header like `"setup 2/5"` or `"provision 3/8"`.
/// `command` is what would be executed.
pub fn prompt_step(label: &str, command: &str) -> anyhow::Result<Decision> {
    let stdin = std::io::stdin();
    let stderr = std::io::stderr();
    prompt_step_io(stdin.lock(), stderr.lock(), label, command)
}

/// Same as [`prompt_step`] but takes explicit reader and writer for testing.
pub fn prompt_step_io<R: BufRead, W: Write>(
    mut reader: R,
    mut writer: W,
    label: &str,
    command: &str,
) -> anyhow::Result<Decision> {
    writeln!(writer, "\n  → {label}")?;
    for line in command.lines() {
        writeln!(writer, "      {line}")?;
    }

    loop {
        write!(writer, "  Run? [Y/n/e/a/q]: ")?;
        writer.flush()?;

        let mut answer = String::new();
        let n = reader
            .read_line(&mut answer)
            .context("failed to read from stdin")?;
        if n == 0 {
            // EOF — treat as quit so we don't loop forever in scripts.
            return Ok(Decision::Quit);
        }
        let trimmed = answer.trim().to_ascii_lowercase();

        match trimmed.as_str() {
            "" | "y" | "yes" => return Ok(Decision::Run(command.to_string())),
            "n" | "no" => return Ok(Decision::Skip),
            "a" | "all" => return Ok(Decision::All(command.to_string())),
            "q" | "quit" => return Ok(Decision::Quit),
            "e" | "edit" => {
                write!(writer, "  Edit (empty = keep): ")?;
                writer.flush()?;
                let mut edited = String::new();
                reader
                    .read_line(&mut edited)
                    .context("failed to read edited command from stdin")?;
                let edited = edited.trim();
                if edited.is_empty() {
                    return Ok(Decision::Run(command.to_string()));
                }
                return Ok(Decision::Run(edited.to_string()));
            }
            _ => {
                writeln!(writer, "  Please answer y, n, e, a, or q.")?;
            }
        }
    }
}

/// Bail out with a "user quit" error.
#[must_use]
pub fn user_quit_error() -> anyhow::Error {
    anyhow::anyhow!("aborted by user (interactive q)")
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn run(input: &str, command: &str) -> Decision {
        let reader = Cursor::new(input.as_bytes());
        let writer = Vec::new();
        prompt_step_io(reader, writer, "test 1/1", command).unwrap()
    }

    #[test]
    fn user_quit_error_message() {
        let e = user_quit_error();
        assert!(format!("{e}").contains("aborted by user"));
    }

    #[test]
    fn yes_runs_command_unchanged() {
        match run("y\n", "echo hi") {
            Decision::Run(cmd) => assert_eq!(cmd, "echo hi"),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn empty_default_is_yes() {
        match run("\n", "echo hi") {
            Decision::Run(cmd) => assert_eq!(cmd, "echo hi"),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn uppercase_yes_works() {
        assert!(matches!(run("YES\n", "x"), Decision::Run(_)));
    }

    #[test]
    fn n_skips() {
        assert!(matches!(run("n\n", "x"), Decision::Skip));
    }

    #[test]
    fn a_runs_all() {
        match run("a\n", "echo hi") {
            Decision::All(cmd) => assert_eq!(cmd, "echo hi"),
            other => panic!("expected All, got {other:?}"),
        }
    }

    #[test]
    fn q_quits() {
        assert!(matches!(run("q\n", "x"), Decision::Quit));
    }

    #[test]
    fn edit_replaces_command() {
        match run("e\necho replaced\n", "echo original") {
            Decision::Run(cmd) => assert_eq!(cmd, "echo replaced"),
            other => panic!("expected Run with edited command, got {other:?}"),
        }
    }

    #[test]
    fn edit_empty_keeps_original() {
        match run("e\n\n", "echo original") {
            Decision::Run(cmd) => assert_eq!(cmd, "echo original"),
            other => panic!("expected Run with original, got {other:?}"),
        }
    }

    #[test]
    fn invalid_then_valid() {
        // First answer invalid, prompt again, then valid.
        match run("z\ny\n", "echo hi") {
            Decision::Run(cmd) => assert_eq!(cmd, "echo hi"),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn eof_quits() {
        // No newline, just empty input — read_line returns 0.
        assert!(matches!(run("", "x"), Decision::Quit));
    }
}

