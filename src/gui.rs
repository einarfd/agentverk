//! `agv gui` — open the VM's desktop in the host browser.
//!
//! Relies on the `gui-xfce` mixin (or any mixin that serves an HTML5 VNC
//! client over HTTP and declares `[auto_forwards.gui]`). agv allocates a
//! free host port at VM start; this module reads that port and opens
//! `http://127.0.0.1:<port>/vnc.html?autoconnect=1&resize=scale` in the
//! system default browser.
//!
//! The VNC server inside the guest binds `127.0.0.1` only with
//! `-SecurityTypes None`. The only way to reach it is through the
//! SSH-tunnel supervisor that the `[auto_forwards.gui]` plumbing spawns,
//! and that tunnel is gated by the VM's unique ed25519 SSH key. So no
//! password ever hits the URL, the browser history, or local storage —
//! the SSH tunnel is the auth boundary.

use anyhow::{bail, Context as _};

use crate::config;
use crate::vm::instance::{Instance, Status};

const AUTO_FORWARD_NAME: &str = "gui";

pub async fn run(name: &str, no_launch: bool) -> anyhow::Result<()> {
    let inst = Instance::open(name)?;
    let status = inst.reconcile_status().await?;
    if status != Status::Running {
        bail!(
            "VM '{name}' is not running (status: {status}). \
             Start it with: agv start {name}"
        );
    }

    let cfg = config::load_resolved(&inst.config_path())?;
    if !cfg.auto_forwards.contains_key(AUTO_FORWARD_NAME) {
        bail!(
            "VM '{name}' has no GUI forward — add a mixin that provides one\n  \
             (e.g. include = [\"gui-xfce\"]) and recreate the VM."
        );
    }

    let port_path = inst.auto_forward_port_path(AUTO_FORWARD_NAME);
    let port_raw = tokio::fs::read_to_string(&port_path).await.with_context(|| {
        format!(
            "failed to read GUI port from {} — is the VM's forward supervisor up?",
            port_path.display()
        )
    })?;
    let port: u16 = port_raw
        .trim()
        .parse()
        .with_context(|| format!("{} did not contain a valid port", port_path.display()))?;

    let url = format!("http://127.0.0.1:{port}/vnc.html?autoconnect=1&resize=scale");

    println!("  VM:   {name}");
    println!("  URL:  {url}");

    if !no_launch {
        if let Err(e) = open_in_browser(&url) {
            // Browser launch is a convenience — don't fail out if no
            // handler is registered. The URL above is the whole story.
            eprintln!();
            eprintln!("  ! Could not open the browser: {e:#}");
            eprintln!("  Open the URL above manually.");
        }
    }

    Ok(())
}

fn open_in_browser(url: &str) -> anyhow::Result<()> {
    let tool = launcher_tool();
    let output = std::process::Command::new(tool)
        .arg(url)
        .output()
        .with_context(|| format!("failed to run {tool} {url}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{tool} failed (exit {}): {stderr}", output.status);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn launcher_tool() -> &'static str {
    "open"
}

#[cfg(all(unix, not(target_os = "macos")))]
fn launcher_tool() -> &'static str {
    "xdg-open"
}

#[cfg(not(unix))]
fn launcher_tool() -> &'static str {
    "cmd"
}

#[cfg(test)]
mod tests {
    #[test]
    fn launcher_tool_is_defined_for_this_platform() {
        // Compile-time check that at least one cfg branch picks a tool.
        let tool = super::launcher_tool();
        assert!(!tool.is_empty());
    }
}
