//! Integration tests for the agv CLI.
//!
//! These tests exercise the compiled binary to verify argument parsing,
//! help output, and commands that require no external tools.

use assert_cmd::Command;
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
        .stdout(contains("specs"));
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
