//! First-boot provisioning pipeline — what happens after QEMU is up and SSH answers.
//!
//! `run_first_boot` is the top-level entry point, invoked by `vm::create`
//! (with `--start`), `vm::start` (first boot or `--retry`), and by the
//! template subsystem (during source-VM provisioning). It reads the saved
//! [`ProvisionState`] and resumes from the recorded phase/index so a failed
//! first boot can pick up where it left off.
//!
//! Helpers kept local to this module:
//! - `append_provision_log` — append output to `provision.log`.
//! - `copy_files` / `run_setup` / `run_provision_steps` — the three work phases.
//! - `step_label` / `shell_escape` / `parent_dir_of` — small pure helpers.

use std::path::Path;
use std::time::Duration;

use anyhow::Context as _;
use indicatif::ProgressBar;
use tracing::info;

use crate::config::{FileEntry, ProvisionStep, ResolvedConfig};
use crate::interactive::{self, InteractiveState};
use crate::ssh;

use super::instance::{Instance, Phase, ProvisionState};
use super::step_done;

/// Append output text to the provision log file.
async fn append_provision_log(instance: &Instance, text: &str) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt as _;
    let path = instance.provision_log_path();
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
        .with_context(|| format!("failed to open provision log {}", path.display()))?;
    file.write_all(text.as_bytes()).await?;
    Ok(())
}

/// Execute setup steps as root via `sudo`. First failure aborts remaining steps.
///
/// SSH connects as the configured user (the only user with an authorized key)
/// and wraps each command with `sudo` to gain root privileges.
/// Output is captured to `provision.log`; with `verbose`, also written to stderr.
#[expect(
    clippy::too_many_arguments,
    reason = "internal helper threading instance + user + steps + start_index + interactive state + spinner; the parameters are distinct and refactoring would just shuffle them"
)]
async fn run_setup(
    instance: &Instance,
    user: &str,
    steps: &[ProvisionStep],
    start_index: usize,
    interactive_mode: bool,
    int_state: &mut InteractiveState,
    verbose: bool,
    spinner: &ProgressBar,
) -> anyhow::Result<()> {
    let total = steps.len();
    for (i, step) in steps.iter().enumerate().skip(start_index) {
        instance
            .write_provision_state(&ProvisionState {
                phase: Phase::Setup,
                index: i,
                total,
                error: None,
            })
            .await?;

        let num = i + 1;
        let label = step_label(step);
        spinner.set_message(format!("Running setup ({num}/{total}): {label}..."));

        if let Some(ref script) = step.run {
            // Interactive prompt — may edit the inline script.
            let to_run = if interactive_mode && !int_state.all {
                let decision = spinner.suspend(|| {
                    interactive::prompt_step(&format!("setup {num}/{total}"), script)
                })?;
                match decision {
                    interactive::Decision::Run(cmd) => cmd,
                    interactive::Decision::All(cmd) => {
                        int_state.all = true;
                        cmd
                    }
                    interactive::Decision::Skip => continue,
                    interactive::Decision::Quit => return Err(interactive::user_quit_error()),
                }
            } else {
                script.clone()
            };

            info!(step = num, "running inline setup script (as root)");
            let output = ssh::run_cmd(
                instance,
                user,
                &[format!("sudo bash -c {}", shell_escape(&to_run))],
            )
            .await
            .with_context(|| format!("setup step {num}: inline script failed"))?;
            append_provision_log(instance, &format!("=== setup step {num} ({label}) ===\n{output}")).await?;
            if verbose {
                eprint!("{output}");
            }
        } else if let Some(ref script_path) = step.script {
            // Interactive prompt — script files can't be edited inline.
            if interactive_mode && !int_state.all {
                let display = format!("(script file) {script_path}");
                let decision = spinner.suspend(|| {
                    interactive::prompt_step(&format!("setup {num}/{total}"), &display)
                })?;
                match decision {
                    interactive::Decision::Run(_) => {}
                    interactive::Decision::All(_) => int_state.all = true,
                    interactive::Decision::Skip => {
                        step_done(spinner, &format!("Setup ({num}/{total}): {label} (skipped)"));
                        continue;
                    }
                    interactive::Decision::Quit => return Err(interactive::user_quit_error()),
                }
            }

            info!(step = num, path = script_path, "running setup script file (as root)");
            let remote_path = format!("/tmp/agv-setup-{i}.sh");

            ssh::copy_to(instance, user, Path::new(script_path), &remote_path)
                .await
                .with_context(|| {
                    format!("setup step {num}: failed to copy script {script_path}")
                })?;

            let output = ssh::run_cmd(
                instance,
                user,
                &[format!(
                    "sudo bash -c {}",
                    shell_escape(&format!("chmod +x {remote_path} && {remote_path}"))
                )],
            )
            .await
            .with_context(|| {
                format!("setup step {num}: script {script_path} failed")
            })?;
            append_provision_log(instance, &format!("=== setup step {num} ({label}) ===\n{output}")).await?;
            if verbose {
                eprint!("{output}");
            }
        }

        step_done(spinner, &format!("Setup ({num}/{total}): {label}"));
    }

    info!("setup complete");
    Ok(())
}

/// Execute provisioning steps in order. First failure aborts remaining steps.
///
/// Output is captured to `provision.log`; with `verbose`, also written to stderr.
#[expect(
    clippy::too_many_arguments,
    reason = "internal helper threading instance + user + steps + start_index + interactive state + spinner; the parameters are distinct and refactoring would just shuffle them"
)]
async fn run_provision_steps(
    instance: &Instance,
    user: &str,
    steps: &[ProvisionStep],
    start_index: usize,
    interactive_mode: bool,
    int_state: &mut InteractiveState,
    verbose: bool,
    spinner: &ProgressBar,
) -> anyhow::Result<()> {
    let total = steps.len();
    for (i, step) in steps.iter().enumerate().skip(start_index) {
        instance
            .write_provision_state(&ProvisionState {
                phase: Phase::Provision,
                index: i,
                total,
                error: None,
            })
            .await?;

        let num = i + 1;
        let label = step_label(step);
        spinner.set_message(format!("Running provision ({num}/{total}): {label}..."));

        if let Some(ref script) = step.run {
            // Interactive prompt — may edit the inline script.
            let to_run = if interactive_mode && !int_state.all {
                let decision = spinner.suspend(|| {
                    interactive::prompt_step(&format!("provision {num}/{total}"), script)
                })?;
                match decision {
                    interactive::Decision::Run(cmd) => cmd,
                    interactive::Decision::All(cmd) => {
                        int_state.all = true;
                        cmd
                    }
                    interactive::Decision::Skip => continue,
                    interactive::Decision::Quit => return Err(interactive::user_quit_error()),
                }
            } else {
                script.clone()
            };

            info!(step = num, "running inline provisioning script");
            let output = ssh::run_cmd(
                instance,
                user,
                &[format!("bash -c {}", shell_escape(&to_run))],
            )
            .await
            .with_context(|| format!("provisioning step {num}: inline script failed"))?;
            append_provision_log(instance, &format!("=== provision step {num} ({label}) ===\n{output}")).await?;
            if verbose {
                eprint!("{output}");
            }
        } else if let Some(ref script_path) = step.script {
            // Interactive prompt — script files can't be edited inline.
            if interactive_mode && !int_state.all {
                let display = format!("(script file) {script_path}");
                let decision = spinner.suspend(|| {
                    interactive::prompt_step(&format!("provision {num}/{total}"), &display)
                })?;
                match decision {
                    interactive::Decision::Run(_) => {}
                    interactive::Decision::All(_) => int_state.all = true,
                    interactive::Decision::Skip => {
                        step_done(spinner, &format!("Provision ({num}/{total}): {label} (skipped)"));
                        continue;
                    }
                    interactive::Decision::Quit => return Err(interactive::user_quit_error()),
                }
            }

            info!(step = num, path = script_path, "running provisioning script file");
            let remote_path = format!("/tmp/agv-provision-{i}.sh");

            ssh::copy_to(instance, user, Path::new(script_path), &remote_path)
                .await
                .with_context(|| {
                    format!("provisioning step {num}: failed to copy script {script_path}")
                })?;

            let output = ssh::run_cmd(
                instance,
                user,
                &[format!("chmod +x {remote_path} && {remote_path}")],
            )
            .await
            .with_context(|| {
                format!("provisioning step {num}: script {script_path} failed")
            })?;
            append_provision_log(instance, &format!("=== provision step {num} ({label}) ===\n{output}")).await?;
            if verbose {
                eprint!("{output}");
            }
        }

        step_done(spinner, &format!("Provision ({num}/{total}): {label}"));
    }

    info!("provisioning complete");
    Ok(())
}

/// Wait for SSH with a live elapsed-time counter in the spinner message.
pub(super) async fn wait_for_ssh(
    inst: &Instance,
    user: &str,
    spinner: &ProgressBar,
) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    spinner.set_message("Waiting for SSH...");
    let result = tokio::select! {
        result = ssh::wait_for_ready(inst, user) => result,
        _ = async {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                spinner.set_message(format!(
                    "Waiting for SSH... ({}s)",
                    start.elapsed().as_secs()
                ));
            }
        } => unreachable!(),
    };
    result
}

/// Copy `[[files]]` entries into the VM via SCP.
///
/// Creates parent directories for each destination, then copies the file.
/// Errors are reported immediately — unlike cloud-init `write_files`, failures
/// are never silent.
async fn copy_files(
    instance: &Instance,
    user: &str,
    files: &[FileEntry],
    start_index: usize,
    interactive_mode: bool,
    int_state: &mut InteractiveState,
    spinner: &ProgressBar,
) -> anyhow::Result<()> {
    let total = files.len();
    for (i, file) in files.iter().enumerate().skip(start_index) {
        // Update state to "about to run step i".
        instance
            .write_provision_state(&ProvisionState {
                phase: Phase::Files,
                index: i,
                total,
                error: None,
            })
            .await?;

        let num = i + 1;
        let label = file
            .source
            .rsplit('/')
            .next()
            .unwrap_or(&file.source);
        spinner.set_message(format!("Copying file ({num}/{total}): {label}..."));

        // Optional file: skip silently when the source isn't on the host.
        // Lets configs declare opportunistic copies (an SSH key the user
        // may or may not have, a gh config they may or may not have set up)
        // without each missing source aborting the create flow.
        if file.optional && !Path::new(&file.source).exists() {
            tracing::info!(
                source = %file.source,
                "file marked optional, source missing — skipping"
            );
            step_done(
                spinner,
                &format!("Copy ({num}/{total}): {label} (skipped — optional, source not on host)"),
            );
            continue;
        }

        // Interactive prompt: let the user skip a file or quit.
        // Edits don't make sense for file copies.
        if interactive_mode && !int_state.all {
            let display = format!("copy {} → {}", file.source, file.dest);
            let decision = spinner.suspend(|| {
                interactive::prompt_step(&format!("file {num}/{total}"), &display)
            })?;
            match decision {
                interactive::Decision::Run(_) => {}
                interactive::Decision::All(_) => int_state.all = true,
                interactive::Decision::Skip => continue,
                interactive::Decision::Quit => return Err(interactive::user_quit_error()),
            }
        }

        // Ensure the parent directory exists inside the VM.
        let parent_dir = parent_dir_of(&file.dest);
        if !parent_dir.is_empty() && parent_dir != "." && parent_dir != "/" {
            ssh::run_cmd(
                instance,
                user,
                &[format!("mkdir -p {}", shell_escape(parent_dir))],
            )
            .await
            .with_context(|| {
                format!("file {num}: failed to create directory {parent_dir}")
            })?;
        }

        ssh::copy_to(instance, user, Path::new(&file.source), &file.dest)
            .await
            .with_context(|| {
                format!(
                    "file {num}: failed to copy {} → {}",
                    file.source, file.dest
                )
            })?;

        step_done(spinner, &format!("Copied file ({num}/{total}): {label}"));
    }

    Ok(())
}

/// Run the full first-boot provisioning flow: wait for SSH, copy files, setup, provision.
///
/// Called by `create()` (with `--start`) and `start()` (first boot or `--retry`).
/// Reads the existing `provision_state` and resumes from the saved phase/index,
/// so a previously failed run can pick up where it left off without re-running
/// already-completed steps.
///
/// In `interactive_mode`, the user is prompted before each file copy, setup
/// step, and provision step.
pub(super) async fn run_first_boot(
    inst: &Instance,
    config: &ResolvedConfig,
    interactive_mode: bool,
    verbose: bool,
    _quiet: bool,
    spinner: &ProgressBar,
) -> anyhow::Result<()> {
    let state = inst.read_provision_state().await;
    let mut int_state = InteractiveState::new();

    // Always wait for SSH — it's idempotent and we need it for all later phases.
    if state.phase == Phase::SshWait || state.phase == Phase::Files
        || state.phase == Phase::Setup || state.phase == Phase::Provision
    {
        inst.write_provision_state(&ProvisionState {
            phase: Phase::SshWait,
            index: 0,
            total: 0,
            error: None,
        })
        .await?;
        wait_for_ssh(inst, &config.user, spinner).await?;
        step_done(spinner, "SSH is ready");
    }

    // Files phase: skip if we're already past it.
    if state.phase == Phase::SshWait
        || state.phase == Phase::Files
    {
        let files_start = if state.phase == Phase::Files { state.index } else { 0 };
        if !config.files.is_empty() {
            copy_files(
                inst,
                &config.user,
                &config.files,
                files_start,
                interactive_mode,
                &mut int_state,
                spinner,
            )
            .await?;
        }
    }

    // Setup phase.
    if state.phase == Phase::SshWait
        || state.phase == Phase::Files
        || state.phase == Phase::Setup
    {
        let setup_start = if state.phase == Phase::Setup { state.index } else { 0 };
        if !config.setup.is_empty() {
            run_setup(
                inst,
                &config.user,
                &config.setup,
                setup_start,
                interactive_mode,
                &mut int_state,
                verbose,
                spinner,
            )
            .await?;
        }
    }

    // Provision phase.
    let provision_start = if state.phase == Phase::Provision { state.index } else { 0 };
    if !config.provision.is_empty() {
        run_provision_steps(
            inst,
            &config.user,
            &config.provision,
            provision_start,
            interactive_mode,
            &mut int_state,
            verbose,
            spinner,
        )
        .await?;
    }

    // Write ~/.agv/system.md so agents running inside the VM can discover
    // which mixins are applied and any non-obvious wiring they declared.
    // Soft-fail: a write error here does not invalidate a successful boot.
    if let Err(err) = write_system_info(inst, config).await {
        tracing::warn!(vm = %inst.name, error = %format!("{err:#}"), "failed to write ~/.agv/system.md");
    }

    inst.mark_provisioned().await?;

    // Print any human-only manual setup steps (auth flows, etc) the
    // mixins or top-level config flagged. Runs after mark_provisioned so
    // the VM is fully provisioned before the user is told what to do
    // next; printed only on this first successful run, never on later
    // `agv start`s where run_first_boot is skipped.
    crate::manual_steps::print_to_host(config);

    Ok(())
}

/// SSH into the VM and write `~/.agv/system.md` as the default user.
///
/// Uses base64 to embed the markdown body so newlines, quotes, and shell
/// metacharacters inside mixin-authored notes never reach a shell parser.
async fn write_system_info(inst: &Instance, config: &ResolvedConfig) -> anyhow::Result<()> {
    use base64::{engine::general_purpose::STANDARD, Engine as _};

    let body = super::system_info::render(config, std::env::consts::ARCH);
    let encoded = STANDARD.encode(body.as_bytes());
    let cmd = format!(
        "mkdir -p ~/.agv && printf '%s' {} | base64 -d > ~/.agv/system.md",
        shell_escape(&encoded)
    );
    ssh::run_cmd(inst, &config.user, &[cmd])
        .await
        .context("failed to write ~/.agv/system.md via ssh")?;
    Ok(())
}

/// Produce a short label for a provisioning step.
///
/// If the step has a `source` (i.e. it came from an include module), shows that.
/// Otherwise falls back to the script file path or truncated inline script.
fn step_label(step: &ProvisionStep) -> String {
    if let Some(ref source) = step.source {
        return source.clone();
    }
    if let Some(ref path) = step.script {
        return path.clone();
    }
    if let Some(ref script) = step.run {
        let first_line = script.lines().next().unwrap_or(script).trim();
        // Truncate by char (not byte) so a multi-byte char at the 40th
        // position does not panic on slice.
        let mut chars = first_line.chars();
        let truncated: String = chars.by_ref().take(40).collect();
        return if chars.next().is_some() {
            format!("{truncated}...")
        } else {
            truncated
        };
    }
    "unknown".to_string()
}

/// Single-quote a string for safe embedding in a shell command.
///
/// Wraps the value in single quotes and escapes any embedded single quotes
/// using the `'\''` idiom (end quote, escaped quote, reopen quote).
fn shell_escape(s: &str) -> String {
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

/// Extract the parent directory from a destination path.
///
/// Returns `"."` when no slash is present, or the portion before the last
/// slash. Used by `copy_files()` to `mkdir -p` before copying.
fn parent_dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or(".", |(dir, _)| dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_dir_of_absolute_path() {
        assert_eq!(parent_dir_of("/home/agent/.ssh/id_ed25519"), "/home/agent/.ssh");
    }

    #[test]
    fn parent_dir_of_root_file() {
        assert_eq!(parent_dir_of("/file.txt"), "");
    }

    #[test]
    fn parent_dir_of_home_file() {
        assert_eq!(parent_dir_of("/home/agent/file.txt"), "/home/agent");
    }

    #[test]
    fn parent_dir_of_no_slash() {
        assert_eq!(parent_dir_of("file.txt"), ".");
    }

    #[test]
    fn parent_dir_of_nested() {
        assert_eq!(parent_dir_of("/a/b/c/d"), "/a/b/c");
    }

    // -----------------------------------------------------------------------
    // shell_escape — correctness + shell-injection safety
    // -----------------------------------------------------------------------

    #[test]
    fn shell_escape_empty_string() {
        assert_eq!(shell_escape(""), "''");
    }

    #[test]
    fn shell_escape_plain_ascii() {
        assert_eq!(shell_escape("hello"), "'hello'");
    }

    #[test]
    fn shell_escape_with_spaces() {
        assert_eq!(shell_escape("hello world"), "'hello world'");
    }

    #[test]
    fn shell_escape_single_quote_uses_end_escape_reopen_idiom() {
        // The idiom: close quote, escaped quote, reopen quote.
        assert_eq!(shell_escape("it's"), r"'it'\''s'");
    }

    #[test]
    fn shell_escape_multiple_single_quotes() {
        assert_eq!(shell_escape("a'b'c"), r"'a'\''b'\''c'");
    }

    #[test]
    fn shell_escape_quote_at_start() {
        assert_eq!(shell_escape("'foo"), r"''\''foo'");
    }

    #[test]
    fn shell_escape_quote_at_end() {
        assert_eq!(shell_escape("foo'"), r"'foo'\'''");
    }

    #[test]
    fn shell_escape_only_a_single_quote() {
        assert_eq!(shell_escape("'"), r"''\'''");
    }

    /// The actual security property: the escaped form, embedded in a
    /// `sh -c '<escaped>'` command, must produce the original string.
    /// This covers shell metacharacters — `$`, `` ` ``, `;`, `|`, `*`, `&`,
    /// newline, backslash — which should all pass through literally.
    #[test]
    fn shell_escape_roundtrips_through_sh_for_malicious_inputs() {
        let inputs = [
            "hello",
            "hello world",
            "it's fine",
            "$HOME should not expand",
            "`whoami` should not run",
            "$(whoami) should not run",
            "; rm -rf / ; echo pwned",
            "a | b & c && d || e",
            "wild*card?[set]",
            "back\\slash",
            "new\nline\tstuff",
            "mixed 'quotes\" and `backticks` and $vars",
        ];

        for input in inputs {
            let escaped = shell_escape(input);
            let script = format!("printf '%s' {escaped}");
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(&script)
                .output()
                .expect("failed to spawn sh");
            assert!(
                output.status.success(),
                "sh exited non-zero for input {input:?}: stderr={}",
                String::from_utf8_lossy(&output.stderr)
            );
            let got = String::from_utf8(output.stdout)
                .expect("sh stdout was not utf8");
            assert_eq!(
                got, input,
                "shell round-trip mismatch for input {input:?} (escaped as {escaped:?})"
            );
        }
    }

    // -----------------------------------------------------------------------
    // step_label — precedence, truncation, UTF-8 safety
    // -----------------------------------------------------------------------

    fn step(
        source: Option<&str>,
        script: Option<&str>,
        run: Option<&str>,
    ) -> ProvisionStep {
        ProvisionStep {
            source: source.map(str::to_string),
            script: script.map(str::to_string),
            run: run.map(str::to_string),
        }
    }

    #[test]
    fn step_label_source_wins_over_everything() {
        let s = step(Some("claude"), Some("./bootstrap.sh"), Some("echo hi"));
        assert_eq!(step_label(&s), "claude");
    }

    #[test]
    fn step_label_script_wins_over_run() {
        let s = step(None, Some("./bootstrap.sh"), Some("echo hi"));
        assert_eq!(step_label(&s), "./bootstrap.sh");
    }

    #[test]
    fn step_label_short_run_returned_as_is() {
        let s = step(None, None, Some("apt-get install -y ripgrep"));
        assert_eq!(step_label(&s), "apt-get install -y ripgrep");
    }

    #[test]
    fn step_label_long_run_is_truncated_with_ellipsis() {
        // 41-char input: "a" repeated 41 times.
        let long = "a".repeat(41);
        let s = step(None, None, Some(&long));
        assert_eq!(step_label(&s), format!("{}...", "a".repeat(40)));
    }

    #[test]
    fn step_label_exactly_40_chars_not_truncated() {
        let exact = "a".repeat(40);
        let s = step(None, None, Some(&exact));
        assert_eq!(step_label(&s), exact);
    }

    #[test]
    fn step_label_multiline_run_takes_first_line_only() {
        let s = step(None, None, Some("cd /tmp\n./build.sh"));
        assert_eq!(step_label(&s), "cd /tmp");
    }

    #[test]
    fn step_label_trims_leading_and_trailing_whitespace() {
        let s = step(None, None, Some("   echo hi   "));
        assert_eq!(step_label(&s), "echo hi");
    }

    #[test]
    fn step_label_none_everywhere_is_unknown() {
        let s = step(None, None, None);
        assert_eq!(step_label(&s), "unknown");
    }

    /// Regression test: `&first_line[..40]` byte-sliced the string, which
    /// panicked when byte 40 fell inside a multi-byte UTF-8 char. Truncation
    /// is now char-based.
    #[test]
    fn step_label_does_not_panic_on_utf8_boundary() {
        // "é" is 2 bytes. Put 39 ASCII chars followed by "é" + filler: byte 40
        // lands mid-char. Total chars > 40 so truncation kicks in.
        let tricky = format!("{}é{}", "a".repeat(39), "b".repeat(20));
        let s = step(None, None, Some(&tricky));
        let label = step_label(&s);
        assert!(
            label.ends_with("..."),
            "expected truncation ellipsis, got {label:?}"
        );
        assert!(
            label.chars().count() <= 43, // 40 chars + "..."
            "label too long: {label:?}"
        );
    }
}
