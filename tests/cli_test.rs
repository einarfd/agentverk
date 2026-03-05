//! Integration tests for the agv CLI.
//!
//! These tests exercise the compiled binary to verify argument parsing,
//! help output, and commands that require no external tools.

use assert_cmd::Command;
use predicates::prelude::*;
use predicates::str::contains;

fn agv() -> Command {
    #[allow(deprecated)]
    Command::cargo_bin("agv").unwrap()
}

// ── Help and version ─────────────────────────────────────────────────────────

#[test]
fn help_flag_succeeds() {
    agv().arg("--help").assert().success();
}

#[test]
fn help_lists_all_subcommands() {
    agv()
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("create"))
        .stdout(contains("start"))
        .stdout(contains("stop"))
        .stdout(contains("destroy"))
        .stdout(contains("ssh"))
        .stdout(contains("ls"))
        .stdout(contains("images"))
        .stdout(contains("inspect"))
        .stdout(contains("template"))
        .stdout(contains("cache"))
        .stdout(contains("specs"))
        .stdout(contains("config"))
        .stdout(contains("doctor"))
        .stdout(contains("init"));
}

#[test]
fn version_flag_succeeds() {
    agv().arg("--version").assert().success();
}

// ── Missing / unknown subcommand ─────────────────────────────────────────────

#[test]
fn no_subcommand_fails() {
    agv().assert().failure();
}

#[test]
fn unknown_subcommand_fails() {
    agv().arg("notacommand").assert().failure();
}

// ── Global flags are accepted ─────────────────────────────────────────────────

#[test]
fn verbose_flag_is_accepted() {
    // `agv --verbose ls` should not fail due to unknown flag.
    agv().args(["--verbose", "ls"]).assert().success();
}

#[test]
fn quiet_flag_is_accepted() {
    agv().args(["--quiet", "ls"]).assert().success();
}

#[test]
fn json_flag_is_accepted() {
    agv().args(["--json", "ls"]).assert().success();
}

// ── Commands that need no VMs or external tools ───────────────────────────────

#[test]
fn ls_succeeds_with_no_vms() {
    agv().arg("ls").assert().success();
}

#[test]
fn images_succeeds_and_lists_builtins() {
    agv()
        .arg("images")
        .assert()
        .success()
        .stdout(contains("ubuntu-24.04"));
}

#[test]
fn cache_ls_succeeds() {
    agv().args(["cache", "ls"]).assert().success();
}

#[test]
fn template_ls_succeeds_with_no_templates() {
    agv().args(["template", "ls"]).assert().success();
}

#[test]
fn specs_succeeds_and_lists_builtins() {
    agv()
        .arg("specs")
        .assert()
        .success()
        .stdout(contains("small"))
        .stdout(contains("medium"))
        .stdout(contains("large"))
        .stdout(contains("xlarge"));
}

// ── Config command ────────────────────────────────────────────────────────────

#[test]
fn config_show_without_name_fails() {
    agv().args(["config", "show"]).assert().failure();
}

#[test]
fn config_show_help_succeeds() {
    agv().args(["config", "show", "--help"]).assert().success();
}

#[test]
fn config_set_without_name_fails() {
    agv().args(["config", "set"]).assert().failure();
}

#[test]
fn config_help_succeeds() {
    agv().args(["config", "--help"]).assert().success();
}

#[test]
fn config_set_help_succeeds() {
    agv().args(["config", "set", "--help"]).assert().success();
}

// ── Subcommand help ───────────────────────────────────────────────────────────

#[test]
fn create_help_succeeds() {
    agv().args(["create", "--help"]).assert().success();
}

#[test]
fn template_help_succeeds() {
    agv().args(["template", "--help"]).assert().success();
}

#[test]
fn cache_help_succeeds() {
    agv().args(["cache", "--help"]).assert().success();
}

// ── Missing required arguments ────────────────────────────────────────────────

#[test]
fn create_without_name_fails() {
    agv().arg("create").assert().failure();
}

#[test]
fn start_without_name_fails() {
    agv().arg("start").assert().failure();
}

#[test]
fn stop_without_name_fails() {
    agv().arg("stop").assert().failure();
}

#[test]
fn destroy_without_name_fails() {
    agv().arg("destroy").assert().failure();
}

#[test]
fn ssh_without_name_fails() {
    agv().arg("ssh").assert().failure();
}

#[test]
fn ssh_help_succeeds() {
    agv().args(["ssh", "--help"]).assert().success();
}

// These tests verify that ssh flags are accepted by clap (not treated as
// unknown agv args). They fail with "VM not found", not a parse error.

#[test]
fn ssh_flag_agent_forwarding_accepted() {
    agv()
        .args(["ssh", "novm", "-A"])
        .assert()
        .failure()
        .stderr(contains("novm").or(contains("not found")).or(contains("No such")));
}

#[test]
fn ssh_flag_port_forward_accepted() {
    agv()
        .args(["ssh", "novm", "-L", "8080:localhost:8080"])
        .assert()
        .failure()
        .stderr(contains("novm").or(contains("not found")).or(contains("No such")));
}

#[test]
fn ssh_command_after_separator_accepted() {
    agv()
        .args(["ssh", "novm", "--", "ls", "-la"])
        .assert()
        .failure()
        .stderr(contains("novm").or(contains("not found")).or(contains("No such")));
}

#[test]
fn ssh_opts_and_command_accepted() {
    agv()
        .args(["ssh", "novm", "-A", "--", "ls"])
        .assert()
        .failure()
        .stderr(contains("novm").or(contains("not found")).or(contains("No such")));
}

#[test]
fn inspect_without_name_fails() {
    agv().arg("inspect").assert().failure();
}

#[test]
fn template_create_without_args_fails() {
    agv().args(["template", "create"]).assert().failure();
}

#[test]
fn template_rm_without_name_fails() {
    agv().args(["template", "rm"]).assert().failure();
}

// ── Doctor ────────────────────────────────────────────────────────────────────

#[test]
fn doctor_succeeds() {
    agv().arg("doctor").assert().success();
}

// ── Init ──────────────────────────────────────────────────────────────────────

#[test]
fn init_help_succeeds() {
    agv().args(["init", "--help"]).assert().success();
}

#[test]
fn init_writes_agv_toml() {
    let dir = tempfile::tempdir().unwrap();
    agv()
        .arg("init")
        .current_dir(&dir)
        .assert()
        .success()
        .stdout(contains("agv.toml"));
    assert!(dir.path().join("agv.toml").exists());
}

#[test]
fn init_template_claude_writes_agv_toml() {
    let dir = tempfile::tempdir().unwrap();
    agv()
        .args(["init", "claude"])
        .current_dir(&dir)
        .assert()
        .success();
    let content = std::fs::read_to_string(dir.path().join("agv.toml")).unwrap();
    assert!(content.contains("claude"));
}

#[test]
fn init_fails_if_agv_toml_exists() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("agv.toml"), "# existing").unwrap();
    agv()
        .arg("init")
        .current_dir(&dir)
        .assert()
        .failure()
        .stderr(contains("already exists"));
}

#[test]
fn init_force_overwrites() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("agv.toml"), "# existing").unwrap();
    agv()
        .args(["init", "--force"])
        .current_dir(&dir)
        .assert()
        .success();
}

#[test]
fn init_unknown_template_fails() {
    let dir = tempfile::tempdir().unwrap();
    agv()
        .args(["init", "bogus"])
        .current_dir(&dir)
        .assert()
        .failure()
        .stderr(contains("unknown template"));
}

// ── Conflicting flags ─────────────────────────────────────────────────────────

#[test]
fn create_from_and_config_conflict() {
    agv()
        .args(["create", "--from", "mytemplate", "--config", "agv.toml", "myvm"])
        .assert()
        .failure();
}

#[test]
fn create_from_and_image_conflict() {
    agv()
        .args(["create", "--from", "mytemplate", "--image", "ubuntu-24.04", "myvm"])
        .assert()
        .failure();
}
