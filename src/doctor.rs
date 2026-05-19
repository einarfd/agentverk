//! Dependency check — `agv doctor`.
//!
//! Searches PATH for every external tool `agv` depends on and reports
//! what is missing together with platform-specific install instructions.

use anstyle::{AnsiColor, Style};
use serde::Serialize;

const GREEN: Style = AnsiColor::Green.on_default();
const RED: Style = AnsiColor::Red.on_default();
const YELLOW: Style = AnsiColor::Yellow.on_default();

struct Check {
    label: &'static str,
    /// Binary names to search — the check passes if *any* candidate is found.
    candidates: Vec<&'static str>,
    install_hint: &'static str,
    /// When `true`, a missing tool counts toward the `issues` total
    /// and surfaces as a red ✗. When `false`, it's a soft "you might
    /// want this" — yellow `~`, doesn't fail the run. Used on macOS
    /// Apple Silicon, where the default backend is `avf` and QEMU
    /// tools (`qemu-system-*`, `qemu-img`) are only needed if the
    /// user explicitly opts into the qemu backend.
    required: bool,
}

// ---------------------------------------------------------------------------
// Platform-specific install hints
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
const QEMU_HINT: &str = "brew install qemu              (Homebrew)\n\
                          sudo port install qemu         (MacPorts)\n\
                          \n\
                          No Homebrew? https://brew.sh";

#[cfg(target_os = "linux")]
const QEMU_HINT: &str = "sudo apt install qemu-system   (Debian/Ubuntu)\n\
                          sudo dnf install qemu-kvm      (Fedora/RHEL)";

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
const QEMU_HINT: &str = "install QEMU for your platform";

#[cfg(target_os = "macos")]
const AVF_RUNNER_HINT: &str = "agv-avf-runner powers `backend = \"avf\"` (Apple Virtualization).\n\
                                If you installed agv from a release tarball, your install is\n\
                                incomplete — the runner should ship alongside the agv binary.\n\
                                If you're building from source: `just build-avf-runner` from the\n\
                                agv repo, then move the resulting `.build/release/agv-avf-runner`\n\
                                next to your installed agv binary (or set AGV_AVF_RUNNER to its\n\
                                path).\n\
                                On Linux this check doesn't apply — AVF is macOS-only and the\n\
                                QEMU backend is used regardless.";

#[cfg(target_os = "macos")]
const OPENSSH_HINT: &str = "OpenSSH is bundled with macOS — check your PATH";

#[cfg(target_os = "linux")]
const OPENSSH_HINT: &str = "sudo apt install openssh-client   (Debian/Ubuntu)";

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
const OPENSSH_HINT: &str = "install OpenSSH for your platform";

#[cfg(target_os = "linux")]
const ISO_HINT: &str = "sudo apt install genisoimage   (Debian/Ubuntu)\n\
                         sudo dnf install genisoimage   (Fedora/RHEL)";

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
const ISO_HINT: &str = "install mkisofs or genisoimage for your platform";

// ---------------------------------------------------------------------------
// Check list
// ---------------------------------------------------------------------------

fn all_checks() -> Vec<Check> {
    // The QEMU system binary is arch-specific at build time.
    let qemu_bin = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "qemu-system-x86_64"
    } else {
        "qemu-system-aarch64"
    };

    // On macOS Apple Silicon, AVF is the default backend
    // (`config::default_backend()` returns `"avf"`). QEMU is still
    // selectable via `--backend qemu`, but a user who only uses AVF
    // legitimately doesn't need any QEMU tooling — `agv doctor`
    // shouldn't fail on them. Everywhere else, QEMU is the default
    // and these tools are genuinely required.
    let qemu_required = !cfg!(all(target_os = "macos", target_arch = "aarch64"));

    vec![
        Check {
            label: qemu_bin,
            candidates: vec![qemu_bin],
            install_hint: QEMU_HINT,
            required: qemu_required,
        },
        Check {
            label: "qemu-img",
            candidates: vec!["qemu-img"],
            install_hint: QEMU_HINT,
            required: qemu_required,
        },
        Check {
            label: "ssh",
            candidates: vec!["ssh"],
            install_hint: OPENSSH_HINT,
            required: true,
        },
        Check {
            label: "ssh-keygen",
            candidates: vec!["ssh-keygen"],
            install_hint: OPENSSH_HINT,
            required: true,
        },
        Check {
            label: "scp",
            candidates: vec!["scp"],
            install_hint: OPENSSH_HINT,
            required: true,
        },
        #[cfg(target_os = "macos")]
        Check {
            label: "hdiutil",
            candidates: vec!["hdiutil"],
            install_hint: "hdiutil is built into macOS — check your installation",
            required: true,
        },
        #[cfg(target_os = "macos")]
        Check {
            label: "agv-avf-runner",
            // Empty candidates — detection runs through `check_present`,
            // which special-cases this label to use
            // `backend::locate_avf_runner` (env override → sibling of
            // the current agv binary). The runner is not expected on
            // PATH; PATH search would always come up empty.
            candidates: vec![],
            install_hint: AVF_RUNNER_HINT,
            required: true,
        },
        #[cfg(not(target_os = "macos"))]
        Check {
            label: "mkisofs / genisoimage",
            candidates: vec!["mkisofs", "genisoimage"],
            install_hint: ISO_HINT,
            required: true,
        },
    ]
}

// ---------------------------------------------------------------------------
// PATH search
// ---------------------------------------------------------------------------

fn is_available(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(name).is_file())
}

/// Does this dependency check pass?
///
/// Defaults to PATH-based detection — `is_available()` across the
/// check's `candidates` list. One special case: `agv-avf-runner`
/// isn't expected on PATH (the install pattern is "sibling of the
/// agv binary, or `AGV_AVF_RUNNER` env override"), so we route its
/// detection through `backend::locate_avf_runner`.
fn check_present(check: &Check) -> bool {
    #[cfg(target_os = "macos")]
    if check.label == "agv-avf-runner" {
        return crate::vm::backend::locate_avf_runner().is_ok();
    }
    check.candidates.iter().any(|b| is_available(b))
}

// ---------------------------------------------------------------------------
// JSON shapes
// ---------------------------------------------------------------------------

/// One dependency-check result for `agv doctor --json`.
///
/// Stable across the 0.x series — additions OK, removals/renames need
/// a major bump.
#[derive(Debug, Clone, Serialize)]
pub struct CheckJson {
    /// Human label (often the binary name, occasionally a slash-joined
    /// alternates list like `"mkisofs / genisoimage"`).
    pub name: String,
    /// `true` when at least one of the candidate binaries was found on PATH.
    pub found: bool,
    /// `true` when this tool is mandatory on the current host. `false`
    /// for tools that only matter for one of the backends — e.g. on
    /// macOS Apple Silicon, AVF is the default and the QEMU tools
    /// (`qemu-system-*`, `qemu-img`) are only needed if the user
    /// opts into `--backend qemu`. Missing optional tools surface as
    /// a soft warning and do NOT increment `issues`.
    pub required: bool,
}

/// Result of checking the AVF runner's wire-protocol version against
/// what agv expects. Serializes as a tagged object so the shape is
/// self-describing in `--json` output.
///
/// Stable across the 0.x series — additions OK, removals/renames need
/// a major bump.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum RunnerProtocolCheck {
    /// Runner reports the version agv expects.
    Match { version: u32 },
    /// Runner reports a version different from agv's. The user needs
    /// to reinstall so both come from the same build — typically a
    /// partial install (`cargo install agv` upgraded the Rust side
    /// but the runner is still from an older tarball).
    Mismatch { found: u32, expected: u32 },
    /// Could read the runner but couldn't determine its version
    /// (unexpected output, non-zero exit, etc.). Surface as a soft
    /// warning rather than a hard fail — `found` already gates the
    /// hard fail.
    Unreadable { reason: String },
}

/// Aggregate doctor report for `agv doctor --json`.
///
/// Stable across the 0.x series — additions OK, removals/renames need
/// a major bump.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    /// `true` iff every dependency check passed AND the runner's
    /// protocol version matches (when applicable).
    pub ok: bool,
    /// Number of failed dependency checks, plus 1 if the runner's
    /// protocol version is a [`RunnerProtocolCheck::Mismatch`]. Does
    /// not include `ssh_include_installed` (the include is
    /// best-effort) or `RunnerProtocolCheck::Unreadable` (a soft
    /// warning).
    pub issues: u32,
    /// One entry per dependency, in the same order as the human output.
    pub checks: Vec<CheckJson>,
    /// `true` if the agv-managed Include line is present in
    /// `~/.ssh/config`. `null` if the host config could not be read.
    pub ssh_include_installed: Option<bool>,
    /// Protocol-version check against the installed
    /// `agv-avf-runner`. `null` when the runner isn't installed or
    /// the host can't run it (non-macOS, etc.) — in those cases the
    /// `agv-avf-runner` entry in `checks` already conveys the issue.
    pub runner_protocol_version: Option<RunnerProtocolCheck>,
}

fn build_report() -> DoctorReport {
    let checks = all_checks();
    let mut entries = Vec::with_capacity(checks.len());
    let mut issues: u32 = 0;
    for check in &checks {
        let found = check_present(check);
        // Only required-and-missing counts as an issue. An optional
        // tool that's absent (e.g. qemu-img on a macOS Apple Silicon
        // AVF-default host where the user never installed QEMU)
        // surfaces as a soft warning instead.
        if !found && check.required {
            issues += 1;
        }
        entries.push(CheckJson {
            name: check.label.to_string(),
            found,
            required: check.required,
        });
    }
    let ssh_include_installed = crate::ssh_config::is_include_installed().ok();
    let runner_protocol_version = check_runner_protocol_version();
    // Bump the issue count for a hard mismatch — same severity as a
    // missing dep: the runner is installed but speaks the wrong wire.
    // `Unreadable` is a soft warning (doesn't increment).
    if let Some(RunnerProtocolCheck::Mismatch { .. }) = runner_protocol_version {
        issues += 1;
    }
    DoctorReport {
        ok: issues == 0,
        issues,
        checks: entries,
        ssh_include_installed,
        runner_protocol_version,
    }
}

/// Probe `agv-avf-runner --version` and compare its reported
/// protocol version against what agv expects.
///
/// Returns `None` when the runner can't be located or we're on a
/// host that doesn't ship the runner — in either case the
/// `agv-avf-runner` dependency-check entry already surfaces the
/// situation, no point double-reporting.
///
/// The runner's `--version` output is `agv-avf-runner protocol <N>`
/// — see [`crate::vm::backend::RUNNER_PROTOCOL_VERSION`] for the
/// reasoning behind printing protocol-only.
#[cfg(target_os = "macos")]
fn check_runner_protocol_version() -> Option<RunnerProtocolCheck> {
    let runner = crate::vm::backend::locate_avf_runner().ok()?;
    let expected = crate::vm::backend::RUNNER_PROTOCOL_VERSION;
    let output = match std::process::Command::new(&runner).arg("--version").output() {
        Ok(o) => o,
        Err(e) => {
            return Some(RunnerProtocolCheck::Unreadable {
                reason: format!("running --version: {e}"),
            });
        }
    };
    if !output.status.success() {
        return Some(RunnerProtocolCheck::Unreadable {
            reason: format!("--version exited with {}", output.status),
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stdout = stdout.trim();
    // Expected shape: `agv-avf-runner protocol <N>`. Anything else
    // is "unreadable" — likely a different binary masquerading
    // under the same name, or a runner from before protocol
    // versioning was wired (which we still want to surface so the
    // user knows to reinstall).
    let Some(suffix) = stdout.strip_prefix("agv-avf-runner protocol ") else {
        return Some(RunnerProtocolCheck::Unreadable {
            reason: format!("unrecognised --version output: {stdout:?}"),
        });
    };
    match suffix.parse::<u32>() {
        Ok(v) if v == expected => Some(RunnerProtocolCheck::Match { version: v }),
        Ok(v) => Some(RunnerProtocolCheck::Mismatch {
            found: v,
            expected,
        }),
        Err(e) => Some(RunnerProtocolCheck::Unreadable {
            reason: format!("parsing version {suffix:?}: {e}"),
        }),
    }
}

#[cfg(not(target_os = "macos"))]
fn check_runner_protocol_version() -> Option<RunnerProtocolCheck> {
    None
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the dependency check and print a report to stdout.
pub fn run() -> anyhow::Result<()> {
    let checks = all_checks();
    let col = checks.iter().map(|c| c.label.len()).max().unwrap_or(0);

    let mut issues: u32 = 0;
    let mut missing_required: Vec<usize> = Vec::new();
    let mut missing_optional: Vec<usize> = Vec::new();

    for (i, check) in checks.iter().enumerate() {
        if check_present(check) {
            anstream::println!("  {:<col$}  {GREEN}✓{GREEN:#}", check.label);
        } else if check.required {
            anstream::println!("  {:<col$}  {RED}✗{RED:#}", check.label);
            issues += 1;
            missing_required.push(i);
        } else {
            // Soft warning — tool is absent but not strictly needed
            // on this host (e.g. qemu-img on a macOS Apple Silicon
            // AVF-default install). Yellow `~` instead of red ✗.
            anstream::println!(
                "  {:<col$}  {YELLOW}~ (optional){YELLOW:#}",
                check.label
            );
            missing_optional.push(i);
        }
    }

    // Runner protocol-version check: only meaningful when the runner
    // itself is present. Surfaced as a sibling status line below the
    // main check list, and a mismatch counts as an issue (same
    // severity as a missing dep — install-skew has the same fix:
    // reinstall to get both binaries from the same build).
    let runner_proto = check_runner_protocol_version();
    let runner_mismatch = matches!(runner_proto, Some(RunnerProtocolCheck::Mismatch { .. }));
    if runner_mismatch {
        issues += 1;
    }

    if issues == 0 {
        anstream::println!();
        anstream::println!("  {GREEN}All required dependencies found.{GREEN:#}");
        print_optional_missing_note(&checks, &missing_optional);
        print_runner_protocol_status(runner_proto.as_ref());
        print_ssh_include_status();
        return Ok(());
    }

    anstream::println!();

    // Print install hints for required-missing, deduplicating when
    // multiple missing tools share the same hint (e.g. qemu-system-*
    // and qemu-img both come from QEMU).
    let mut printed: Vec<&str> = Vec::new();
    for &i in &missing_required {
        let hint = checks[i].install_hint;
        if !printed.contains(&hint) {
            printed.push(hint);
            anstream::println!("  {} — install with:", checks[i].label);
            for line in hint.lines() {
                anstream::println!("    {line}");
            }
            anstream::println!();
        }
    }

    let noun = if issues == 1 { "issue" } else { "issues" };
    anstream::println!("  {YELLOW}{issues} {noun} found.{YELLOW:#}");
    print_optional_missing_note(&checks, &missing_optional);
    print_runner_protocol_status(runner_proto.as_ref());
    print_ssh_include_status();

    Ok(())
}

/// Run the dependency check and emit a JSON report to stdout.
pub fn run_json() -> anyhow::Result<()> {
    let report = build_report();
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

/// Print a one-line summary listing optional tools that are missing,
/// along with the why ("only needed if you use the qemu backend").
/// Skips entirely when nothing optional is missing.
fn print_optional_missing_note(checks: &[Check], missing_optional: &[usize]) {
    if missing_optional.is_empty() {
        return;
    }
    let names: Vec<&str> = missing_optional.iter().map(|&i| checks[i].label).collect();
    anstream::println!();
    anstream::println!(
        "  {YELLOW}Note:{YELLOW:#} optional tool{plural} not found: {names}",
        plural = if names.len() == 1 { "" } else { "s" },
        names = names.join(", "),
    );
    anstream::println!(
        "    Only needed if you create VMs with `--backend qemu`. AVF is the"
    );
    anstream::println!(
        "    default backend on macOS Apple Silicon and doesn't use these tools."
    );
}

/// Append the AVF runner protocol-version status line, when relevant.
///
/// Silent when the check is `None` (runner not installed on this host,
/// or not macOS) — the missing-dep line in the main check list
/// already surfaces "you need to install agv-avf-runner."
fn print_runner_protocol_status(check: Option<&RunnerProtocolCheck>) {
    let Some(check) = check else { return };
    anstream::println!();
    match check {
        RunnerProtocolCheck::Match { version } => {
            anstream::println!(
                "  Runner protocol: {GREEN}✓ v{version}{GREEN:#}"
            );
        }
        RunnerProtocolCheck::Mismatch { found, expected } => {
            anstream::println!(
                "  Runner protocol: {RED}✗ runner v{found}, agv expects v{expected}{RED:#}"
            );
            anstream::println!("    Reinstall so both binaries come from the same build:");
            anstream::println!("      From a clone: `just install`");
            anstream::println!("      From release: grab the matching tarball; the install script handles both");
        }
        RunnerProtocolCheck::Unreadable { reason } => {
            anstream::println!(
                "  Runner protocol: {YELLOW}⚠ unreadable{YELLOW:#}"
            );
            anstream::println!("    {reason}");
        }
    }
}

/// Append the SSH-config-Include status line to the dependency report.
///
/// Called from [`run`] so all doctor output stays in one place. Errors
/// reading the managed config are treated as silent (the user sees no line)
/// — the Include is best-effort and should never cause doctor to fail.
fn print_ssh_include_status() {
    anstream::println!();
    match crate::ssh_config::is_include_installed() {
        Ok(true) => anstream::println!(
            "  SSH config Include: {GREEN}✓ installed{GREEN:#}"
        ),
        Ok(false) => {
            anstream::println!(
                "  SSH config Include: {YELLOW}⚠ not set up{YELLOW:#}"
            );
            anstream::println!("    Run: agv doctor --setup-ssh");
            anstream::println!("    This lets you ssh into VMs by name (e.g. ssh myvm) and");
            anstream::println!("    enables IDE remote development (VS Code, JetBrains, etc.).");
        }
        Err(_) => {}
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Schema pin for `agv doctor --json` — drift here is a major-version
    /// bump.
    #[test]
    fn doctor_report_json_schema_pin() {
        let report = DoctorReport {
            ok: true,
            issues: 0,
            checks: vec![CheckJson {
                name: "qemu-img".to_string(),
                found: true,
                required: true,
            }],
            ssh_include_installed: Some(true),
            runner_protocol_version: Some(RunnerProtocolCheck::Match { version: 1 }),
        };
        let json = serde_json::to_value(&report).unwrap();
        let obj = json.as_object().expect("DoctorReport must serialize as an object");
        let actual: std::collections::BTreeSet<&str> =
            obj.keys().map(String::as_str).collect();
        let expected: std::collections::BTreeSet<&str> = [
            "checks",
            "issues",
            "ok",
            "ssh_include_installed",
            "runner_protocol_version",
        ]
        .into_iter()
        .collect();
        assert_eq!(actual, expected, "DoctorReport JSON keys drifted");
    }

    /// `checks` always serializes as an array, never omitted, even on
    /// platforms with very few checks.
    #[test]
    fn doctor_report_checks_serialize_as_array() {
        let report = DoctorReport {
            ok: true,
            issues: 0,
            checks: vec![],
            ssh_include_installed: None,
            runner_protocol_version: None,
        };
        let json = serde_json::to_value(&report).unwrap();
        let obj = json.as_object().unwrap();
        assert!(obj.get("checks").is_some_and(serde_json::Value::is_array));
        assert_eq!(
            obj.get("ssh_include_installed"),
            Some(&serde_json::Value::Null),
        );
        assert_eq!(
            obj.get("runner_protocol_version"),
            Some(&serde_json::Value::Null),
        );
    }

    /// Schema pin for the `runner_protocol_version` variants. Each
    /// variant must serialize as `{"status": "...", ...}` with a
    /// stable shape per status.
    #[test]
    fn runner_protocol_check_json_shapes() {
        let m = serde_json::to_value(RunnerProtocolCheck::Match { version: 1 }).unwrap();
        assert_eq!(m["status"], "match");
        assert_eq!(m["version"], 1);

        let mismatch =
            serde_json::to_value(RunnerProtocolCheck::Mismatch { found: 0, expected: 1 }).unwrap();
        assert_eq!(mismatch["status"], "mismatch");
        assert_eq!(mismatch["found"], 0);
        assert_eq!(mismatch["expected"], 1);

        let unreadable = serde_json::to_value(RunnerProtocolCheck::Unreadable {
            reason: "x".to_string(),
        })
        .unwrap();
        assert_eq!(unreadable["status"], "unreadable");
        assert_eq!(unreadable["reason"], "x");
    }

    #[test]
    fn check_json_schema_pin() {
        let entry = CheckJson {
            name: "ssh".to_string(),
            found: true,
            required: true,
        };
        let json = serde_json::to_value(&entry).unwrap();
        let obj = json.as_object().unwrap();
        let actual: std::collections::BTreeSet<&str> =
            obj.keys().map(String::as_str).collect();
        let expected: std::collections::BTreeSet<&str> =
            ["found", "name", "required"].into_iter().collect();
        assert_eq!(actual, expected, "CheckJson keys drifted");
    }

    /// macOS `agv doctor` must include the AVF runner in its checks
    /// so users see whether `backend = "avf"` is going to work before
    /// they hit a create-time error. Linux skips this check (AVF is
    /// macOS-only).
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_check_list_includes_avf_runner() {
        let labels: Vec<&str> = all_checks().iter().map(|c| c.label).collect();
        assert!(
            labels.contains(&"agv-avf-runner"),
            "macOS check list must include `agv-avf-runner`; got: {labels:?}"
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_check_list_omits_avf_runner() {
        let labels: Vec<&str> = all_checks().iter().map(|c| c.label).collect();
        assert!(
            !labels.contains(&"agv-avf-runner"),
            "non-macOS check list must NOT include `agv-avf-runner`; got: {labels:?}"
        );
    }
}
