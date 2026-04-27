//! Integration tests for the agv CLI.
//!
//! These tests exercise the compiled binary to verify argument parsing,
//! help output, and commands that require no external tools.

use assert_cmd::Command;
use predicates::prelude::*;
use predicates::str::contains;

fn agv() -> Command {
    #[expect(
        deprecated,
        reason = "assert_cmd's Command::cargo_bin is marked deprecated but is still the documented API"
    )]
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
        .stdout(contains("init"))
        .stdout(contains("cp"))
        .stdout(contains("forward"))
        .stdout(contains("suspend"))
        .stdout(contains("resume"))
        .stdout(contains("rename"));
}

#[test]
fn rename_help_succeeds() {
    agv()
        .args(["rename", "--help"])
        .assert()
        .success()
        .stdout(contains("stopped or suspended"));
}

#[test]
fn rename_missing_args_fails() {
    agv().args(["rename"]).assert().failure();
    agv().args(["rename", "old"]).assert().failure();
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
fn json_flag_is_accepted_on_ls() {
    // Per-command --json (the global form was removed in 0.3 prep —
    // it was defined but never consumed).
    agv().args(["ls", "--json"]).assert().success();
}

#[test]
fn ls_json_emits_an_array_and_no_human_chrome() {
    // Run against an isolated empty data dir so the result is
    // deterministic regardless of what VMs are on the host.
    let tmp = tempfile::tempdir().unwrap();
    let output = agv()
        .env("AGV_DATA_DIR", tmp.path())
        .args(["ls", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success(), "ls --json should succeed");
    let stdout = String::from_utf8(output.stdout).unwrap();

    // Pure JSON — no "No VMs found" banner, no spinner residue.
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
            panic!("ls --json output didn't parse as JSON: {e}\nstdout was:\n{stdout}")
        });
    assert!(parsed.is_array(), "ls --json must emit a JSON array");
    assert_eq!(parsed.as_array().unwrap().len(), 0, "isolated data dir should be empty");
}

/// Catches the "I forgot to register `--json` on this verb" regression.
/// All lifecycle verbs against a VM that doesn't exist will exit non-zero,
/// but they should at least *parse* the flag — clap rejects unknown flags
/// with exit 2 (different from a runtime VM-not-found error). This test
/// just asserts clap is happy.
#[test]
fn json_flag_is_registered_on_every_lifecycle_verb() {
    let tmp = tempfile::tempdir().unwrap();

    // Verbs that take a name + --json. Each errors at runtime because the
    // VM doesn't exist, but the error must come from agv's runtime path,
    // not clap's flag parsing.
    let cases: &[&[&str]] = &[
        &["start", "--json", "agv-no-such-vm-12345"],
        &["stop", "--json", "--force", "agv-no-such-vm-12345"],
        &["suspend", "--json", "agv-no-such-vm-12345"],
        &["resume", "--json", "agv-no-such-vm-12345"],
        &["destroy", "--json", "--force", "agv-no-such-vm-12345"],
        &[
            "rename",
            "--json",
            "agv-no-such-vm-12345",
            "agv-no-such-vm-67890",
        ],
    ];

    for args in cases {
        let output = agv()
            .env("AGV_DATA_DIR", tmp.path())
            .args(*args)
            .output()
            .unwrap();
        // clap returns exit 2 for usage errors (unknown flag / bad args).
        // Anything else means clap accepted the flag and the command ran
        // (then errored at runtime, which is fine — we're not testing
        // success here, just flag registration).
        let code = output.status.code().unwrap_or(-1);
        assert!(
            code != 2,
            "{args:?} exited with code 2 (usage error from clap) — \
             is --json registered on this verb?\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Documented exit codes (see `docs/json-schema.md`):
/// - 0 success
/// - 1 generic
/// - 2 clap usage error
/// - 10 already exists
/// - 11 not found
/// - 12 wrong state
/// - 20 host capacity refused
///
/// These tests verify the codes that are easy to trigger from a clean
/// host (mostly the not-found path). The wrong-state and already-exists
/// codes are exercised by the unit tests in src/error.rs and by the
/// slow-boot integration tests; capacity is exercised at the unit level.
#[test]
fn exit_code_11_for_not_found_commands() {
    let tmp = tempfile::tempdir().unwrap();
    let cases: &[&[&str]] = &[
        &["start", "agv-no-such-vm-12345"],
        &["stop", "agv-no-such-vm-12345"],
        &["suspend", "agv-no-such-vm-12345"],
        &["resume", "agv-no-such-vm-12345"],
        &["inspect", "agv-no-such-vm-12345"],
        &["destroy", "--force", "agv-no-such-vm-12345"],
    ];
    for args in cases {
        let output = agv()
            .env("AGV_DATA_DIR", tmp.path())
            .args(*args)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(11),
            "{args:?} should exit 11 (not found), got {:?}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

#[test]
fn ls_with_label_filter_against_empty_data_dir_returns_empty_json() {
    // No VMs at all → ls --label whatever --json must return [].
    let tmp = tempfile::tempdir().unwrap();
    let output = agv()
        .env("AGV_DATA_DIR", tmp.path())
        .args(["ls", "--label", "session=anything", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(parsed.as_array().map(Vec::len), Some(0));
}

#[test]
fn destroy_without_name_or_label_errors() {
    // `agv destroy` with neither a positional name nor `--label` is a
    // usage error. clap doesn't catch this (both are optional/repeatable),
    // so the runtime check in destroy_command must.
    let tmp = tempfile::tempdir().unwrap();
    let output = agv()
        .env("AGV_DATA_DIR", tmp.path())
        .args(["destroy"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("either a VM name or --label"),
        "expected explanatory error; stderr was: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Smoke-test for the cross-process lock on the image cache. Spawn
/// multiple `agv resources --json` calls in parallel — they all touch
/// the data dir / sysinfo paths and all must produce parseable JSON.
/// This isn't a deep concurrency exercise (resources is read-only on
/// instance state), but it confirms the binary doesn't deadlock or
/// crash when many copies run at once against the same data dir.
#[test]
fn parallel_resources_invocations_all_succeed() {
    let tmp = tempfile::tempdir().unwrap();
    let n = 8;

    let handles: Vec<_> = (0..n)
        .map(|_| {
            let path = tmp.path().to_path_buf();
            std::thread::spawn(move || {
                agv()
                    .env("AGV_DATA_DIR", &path)
                    .args(["resources", "--json"])
                    .output()
                    .unwrap()
            })
        })
        .collect();

    for h in handles {
        let output = h.join().unwrap();
        assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
        let stdout = String::from_utf8(output.stdout).unwrap();
        let _: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    }
}

#[test]
fn destroy_with_label_against_no_matches_succeeds() {
    // No matching VMs is not an error — it's a no-op (idempotent cleanup).
    let tmp = tempfile::tempdir().unwrap();
    let output = agv()
        .env("AGV_DATA_DIR", tmp.path())
        .args(["destroy", "--label", "session=ghost", "--force"])
        .output()
        .unwrap();
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn exit_code_2_for_clap_usage_errors() {
    // Unknown subcommand and missing required arg both go through clap and
    // come back as exit 2 — the conventional Unix usage-error code.
    let exit_code_unknown_cmd = agv()
        .args(["definitely-not-a-subcommand"])
        .output()
        .unwrap()
        .status
        .code();
    assert_eq!(exit_code_unknown_cmd, Some(2));

    let exit_code_missing_arg = agv().arg("inspect").output().unwrap().status.code();
    assert_eq!(exit_code_missing_arg, Some(2));
}

#[test]
fn resources_json_has_expected_top_level_keys() {
    let output = agv()
        .args(["resources", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success(), "resources --json should succeed");
    let stdout = String::from_utf8(output.stdout).unwrap();

    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
            panic!("resources --json output didn't parse as JSON: {e}\nstdout was:\n{stdout}")
        });
    let obj = parsed.as_object().expect("resources --json must emit an object");

    // Top-level shape: { "host": {...}, "allocated": {...} }
    for key in ["host", "allocated"] {
        assert!(obj.contains_key(key), "missing top-level key: {key}");
    }

    // host subkeys (the public agent-readable contract).
    let host = obj["host"].as_object().expect("host must be an object");
    for key in [
        "total_memory_bytes",
        "used_memory_bytes",
        "cpus",
        "data_dir_free_bytes",
    ] {
        assert!(host.contains_key(key), "host missing key: {key}");
    }

    // allocated subkeys.
    let allocated = obj["allocated"].as_object().expect("allocated must be an object");
    for key in [
        "running_memory_bytes",
        "running_cpus",
        "running_count",
        "total_memory_bytes",
        "total_cpus",
        "total_disk_bytes",
        "total_count",
    ] {
        assert!(allocated.contains_key(key), "allocated missing key: {key}");
    }
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

#[test]
fn start_help_mentions_retry_and_interactive() {
    agv()
        .args(["start", "--help"])
        .assert()
        .success()
        .stdout(contains("--retry"))
        .stdout(contains("--interactive"));
}

#[test]
fn create_help_mentions_interactive() {
    agv()
        .args(["create", "--help"])
        .assert()
        .success()
        .stdout(contains("--interactive"));
}

#[test]
fn start_retry_and_interactive_combine() {
    // Both flags together should be accepted by clap (errors with VM-not-found,
    // not with a parse error, proving the flag combination is valid).
    agv()
        .args(["start", "--retry", "--interactive", "novm"])
        .assert()
        .failure()
        .stderr(contains("novm").or(contains("not found")).or(contains("No such")));
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

// ── Suspend / Resume ──────────────────────────────────────────────────────────

#[test]
fn suspend_help_succeeds() {
    agv().args(["suspend", "--help"]).assert().success();
}

#[test]
fn resume_help_succeeds() {
    agv().args(["resume", "--help"]).assert().success();
}

#[test]
fn suspend_without_name_fails() {
    agv().arg("suspend").assert().failure();
}

#[test]
fn resume_without_name_fails() {
    agv().arg("resume").assert().failure();
}

// ── Forward ───────────────────────────────────────────────────────────────────

#[test]
fn forward_help_succeeds() {
    agv().args(["forward", "--help"]).assert().success();
}

#[test]
fn forward_without_ports_fails() {
    agv().args(["forward", "myvm"]).assert().failure();
}

#[test]
fn forward_invalid_port_fails() {
    agv()
        .args(["forward", "novm", "not_a_port"])
        .assert()
        .failure()
        .stderr(contains("not a valid port"));
}

// ── Cp ────────────────────────────────────────────────────────────────────────

#[test]
fn cp_help_succeeds() {
    agv().args(["cp", "--help"]).assert().success();
}

#[test]
fn cp_without_args_fails() {
    agv().arg("cp").assert().failure();
}

#[test]
fn cp_missing_dest_fails() {
    agv().args(["cp", "myvm", ":~/file"]).assert().failure();
}

#[test]
fn cp_no_vm_path_fails() {
    // Neither source nor dest has : prefix — should error.
    agv()
        .args(["cp", "novm", "./a", "./b"])
        .assert()
        .failure()
        .stderr(contains("must be a VM path"));
}

#[test]
fn cp_both_vm_paths_fails() {
    // Both source and dest have : prefix — should error.
    agv()
        .args(["cp", "novm", ":~/a", ":~/b"])
        .assert()
        .failure()
        .stderr(contains("cannot copy between two VM paths"));
}

// ── Init ──────────────────────────────────────────────────────────────────────

#[test]
fn init_help_succeeds() {
    agv().args(["init", "--help"]).assert().success();
}

#[test]
fn init_writes_agv_toml() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("agv.toml");
    agv()
        .args(["init", "-o"])
        .arg(&out)
        .assert()
        .success()
        .stdout(contains("agv.toml"));
    assert!(out.exists());
}

#[test]
fn init_without_output_fails() {
    agv()
        .arg("init")
        .assert()
        .failure()
        .stderr(contains("--output").or(contains("<-o|--output")));
}

#[test]
fn init_template_claude_writes_agv_toml() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("agv.toml");
    agv()
        .args(["init", "claude", "-o"])
        .arg(&out)
        .assert()
        .success();
    let content = std::fs::read_to_string(&out).unwrap();
    assert!(content.contains("claude"));
}

#[test]
fn init_fails_if_agv_toml_exists() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("agv.toml");
    std::fs::write(&out, "# existing").unwrap();
    agv()
        .args(["init", "-o"])
        .arg(&out)
        .assert()
        .failure()
        .stderr(contains("already exists"));
}

#[test]
fn init_force_overwrites() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("agv.toml");
    std::fs::write(&out, "# existing").unwrap();
    agv()
        .args(["init", "--force", "-o"])
        .arg(&out)
        .assert()
        .success();
}

#[test]
fn init_unknown_template_fails() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("agv.toml");
    agv()
        .args(["init", "bogus", "-o"])
        .arg(&out)
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
