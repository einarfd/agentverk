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

    vec![
        Check {
            label: qemu_bin,
            candidates: vec![qemu_bin],
            install_hint: QEMU_HINT,
        },
        Check {
            label: "qemu-img",
            candidates: vec!["qemu-img"],
            install_hint: QEMU_HINT,
        },
        Check {
            label: "ssh",
            candidates: vec!["ssh"],
            install_hint: OPENSSH_HINT,
        },
        Check {
            label: "ssh-keygen",
            candidates: vec!["ssh-keygen"],
            install_hint: OPENSSH_HINT,
        },
        Check {
            label: "scp",
            candidates: vec!["scp"],
            install_hint: OPENSSH_HINT,
        },
        #[cfg(target_os = "macos")]
        Check {
            label: "hdiutil",
            candidates: vec!["hdiutil"],
            install_hint: "hdiutil is built into macOS — check your installation",
        },
        #[cfg(not(target_os = "macos"))]
        Check {
            label: "mkisofs / genisoimage",
            candidates: vec!["mkisofs", "genisoimage"],
            install_hint: ISO_HINT,
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
}

/// Aggregate doctor report for `agv doctor --json`.
///
/// Stable across the 0.x series — additions OK, removals/renames need
/// a major bump.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    /// `true` iff every dependency check passed.
    pub ok: bool,
    /// Number of failed dependency checks. Does not include
    /// `ssh_include_installed` — the include is best-effort.
    pub issues: u32,
    /// One entry per dependency, in the same order as the human output.
    pub checks: Vec<CheckJson>,
    /// `true` if the agv-managed Include line is present in
    /// `~/.ssh/config`. `null` if the host config could not be read.
    pub ssh_include_installed: Option<bool>,
}

fn build_report() -> DoctorReport {
    let checks = all_checks();
    let mut entries = Vec::with_capacity(checks.len());
    let mut issues: u32 = 0;
    for check in &checks {
        let found = check.candidates.iter().any(|b| is_available(b));
        if !found {
            issues += 1;
        }
        entries.push(CheckJson {
            name: check.label.to_string(),
            found,
        });
    }
    let ssh_include_installed = crate::ssh_config::is_include_installed().ok();
    DoctorReport {
        ok: issues == 0,
        issues,
        checks: entries,
        ssh_include_installed,
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the dependency check and print a report to stdout.
pub fn run() -> anyhow::Result<()> {
    let checks = all_checks();
    let col = checks.iter().map(|c| c.label.len()).max().unwrap_or(0);

    let mut issues: u32 = 0;
    let mut missing_indices: Vec<usize> = Vec::new();

    for (i, check) in checks.iter().enumerate() {
        if check.candidates.iter().any(|b| is_available(b)) {
            anstream::println!("  {:<col$}  {GREEN}✓{GREEN:#}", check.label);
        } else {
            anstream::println!("  {:<col$}  {RED}✗{RED:#}", check.label);
            issues += 1;
            missing_indices.push(i);
        }
    }

    if issues == 0 {
        anstream::println!();
        anstream::println!("  {GREEN}All dependencies found.{GREEN:#}");
        print_ssh_include_status();
        return Ok(());
    }

    anstream::println!();

    // Print install hints, deduplicating when multiple missing tools share
    // the same hint (e.g. qemu-system-* and qemu-img both come from QEMU).
    let mut printed: Vec<&str> = Vec::new();
    for &i in &missing_indices {
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
    print_ssh_include_status();

    Ok(())
}

/// Run the dependency check and emit a JSON report to stdout.
pub fn run_json() -> anyhow::Result<()> {
    let report = build_report();
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
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
            }],
            ssh_include_installed: Some(true),
        };
        let json = serde_json::to_value(&report).unwrap();
        let obj = json.as_object().expect("DoctorReport must serialize as an object");
        let actual: std::collections::BTreeSet<&str> =
            obj.keys().map(String::as_str).collect();
        let expected: std::collections::BTreeSet<&str> =
            ["checks", "issues", "ok", "ssh_include_installed"]
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
        };
        let json = serde_json::to_value(&report).unwrap();
        let obj = json.as_object().unwrap();
        assert!(obj.get("checks").is_some_and(serde_json::Value::is_array));
        assert_eq!(
            obj.get("ssh_include_installed"),
            Some(&serde_json::Value::Null),
        );
    }

    #[test]
    fn check_json_schema_pin() {
        let entry = CheckJson {
            name: "ssh".to_string(),
            found: true,
        };
        let json = serde_json::to_value(&entry).unwrap();
        let obj = json.as_object().unwrap();
        let actual: std::collections::BTreeSet<&str> =
            obj.keys().map(String::as_str).collect();
        let expected: std::collections::BTreeSet<&str> =
            ["found", "name"].into_iter().collect();
        assert_eq!(actual, expected, "CheckJson keys drifted");
    }
}
