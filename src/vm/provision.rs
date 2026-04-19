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

    inst.mark_provisioned().await?;
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
        return if first_line.len() > 40 {
            format!("{}...", &first_line[..40])
        } else {
            first_line.to_string()
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
}
