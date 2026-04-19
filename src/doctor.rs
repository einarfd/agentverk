//! Dependency check — `agv doctor`.
//!
//! Searches PATH for every external tool `agv` depends on and reports
//! what is missing together with platform-specific install instructions.

use anstyle::{AnsiColor, Style};

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
