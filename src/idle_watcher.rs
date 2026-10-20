//! Per-VM auto-suspend supervisor.
//!
//! Spawned at `start`/`resume` when the resolved config has
//! `idle_suspend_minutes > 0`, the watcher polls SSH for two activity
//! signals every 60s and triggers `vm::suspend` after the configured
//! number of minutes of confirmed idleness.
//!
//! Idleness is the AND of:
//!   * No tty-bearing line in `who` — no interactive guest sessions.
//!     `-N` SSH supervisors used by port forwards don't allocate a PTY
//!     and don't count, so config-declared forwards never count as
//!     activity by themselves. Newer OpenSSH (Debian 13+ ships 10.x)
//!     also leaves behind utmp entries for the machine sessions agv
//!     itself opens (provisioning commands, this watcher's own probes)
//!     after the connection ends; those carry no tty and must not keep
//!     an idle VM awake forever.
//!   * 5-min load average (from `/proc/loadavg`) below the configured
//!     threshold (default `0.2`). This catches background work like a
//!     `claude` process running in tmux after the user disconnected.
//!
//! Probe errors (sshd hiccup, transient timeout, the watcher's own
//! `cat` losing stdout) are treated as **unknown** — they neither
//! advance nor reset the idle timer. The state-file guard means a long
//! `Configuring` phase during first-boot provisioning never advances
//! the timer either, so the watcher can be spawned at the very start
//! of `start`/`resume` without a special-case grace period.
//!
//! Shape mirrors [`crate::forward_daemon`]: detached child of the agv
//! binary, signal handlers for SIGTERM/SIGINT, single in-process loop.
//! Cleanup paths (`stop`, `suspend`, `destroy`) read the watcher's PID
//! from `<instance>/idle_watcher.pid` and SIGTERM it.

use std::os::unix::process::CommandExt as _;
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use anyhow::Context as _;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{debug, info, warn};

use crate::forward;
use crate::ssh;
use crate::vm;
use crate::vm::instance::{Instance, Status};

/// How often the watcher probes the guest for activity.
///
/// Short enough that crossing the threshold is precise to within a minute;
/// long enough that the constant probing doesn't itself raise guest load.
const PROBE_INTERVAL: Duration = Duration::from_secs(60);

/// Outcome of a single tick's idle evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    Active,
    Idle,
}

/// Pure idle-decision function: given the two probe signals, decide
/// whether this tick counts as idle or active.
///
/// `who_count == 0` AND `loadavg_5m < threshold` → idle. Anything else
/// → active. The threshold is an exclusive lower bound: a load reading
/// exactly equal to the threshold is treated as active, on the
/// principle that a measurable load is some load.
#[must_use]
pub fn evaluate(who_count: u32, loadavg_5m: f32, threshold: f32) -> Activity {
    if who_count == 0 && loadavg_5m < threshold {
        Activity::Idle
    } else {
        Activity::Active
    }
}

/// Detect whether the host was suspended between two probe ticks.
///
/// The watcher schedules each tick exactly `expected` apart on the
/// monotonic clock. When the host suspends (laptop lid closed, sleep)
/// the process freezes — on Linux `tokio::time::sleep` pauses too, on
/// macOS the monotonic clock advances but the process is still frozen.
/// Either way, by the time the tick fires, far more wall-clock has
/// elapsed than the scheduled interval. We use that gap as a proxy:
/// if `now - prev > 2 * expected`, treat it as a host wake event and
/// reset the idle timer in the loop. This matches what tmux, Chromium,
/// IDE plugins and most background-polling tools do — pure heuristic,
/// no OS hooks, no extra deps.
///
/// Returns `false` for backward jumps (NTP step, manual clock change)
/// since `Err(_)` from `duration_since` carries no actionable signal.
#[must_use]
pub fn looks_like_host_wake(prev: SystemTime, now: SystemTime, expected: Duration) -> bool {
    match now.duration_since(prev) {
        Ok(elapsed) => elapsed > expected.saturating_mul(2),
        Err(_) => false,
    }
}

/// Run the idle-watcher supervisor loop until killed by a signal or
/// until it suspends the VM.
///
/// Returns `Ok(())` on clean shutdown (signalled, or after a successful
/// auto-suspend). Recoverable errors during the loop (probe failures,
/// transient suspend errors) are logged but don't terminate the watcher.
pub async fn run(
    vm_name: &str,
    threshold_minutes: u32,
    load_threshold: f32,
) -> anyhow::Result<()> {
    if threshold_minutes == 0 {
        // Defensive: the spawn site already gates on > 0, but if a user
        // hand-invokes the hidden subcommand with 0 we exit cleanly
        // rather than spinning forever.
        info!(vm = vm_name, "idle_suspend_minutes=0 — watcher exiting");
        return Ok(());
    }

    let inst = Instance::open(vm_name)?;
    let user = read_user(&inst)?;
    write_pid_file(&inst).await?;

    let mut term = signal(SignalKind::terminate())
        .context("failed to install SIGTERM handler")?;
    let mut intr =
        signal(SignalKind::interrupt()).context("failed to install SIGINT handler")?;

    let threshold_secs: u64 = u64::from(threshold_minutes) * 60;
    let mut idle_secs: u64 = 0;
    let mut last_tick_wall: SystemTime = SystemTime::now();

    info!(
        vm = vm_name,
        minutes = threshold_minutes,
        threshold = load_threshold,
        "idle watcher started"
    );

    loop {
        // Sleep one tick or break on shutdown.
        tokio::select! {
            () = tokio::time::sleep(PROBE_INTERVAL) => {}
            _ = term.recv() => break,
            _ = intr.recv() => break,
        }

        // Host-suspend heuristic: the tokio sleep above paused while the
        // host was suspended (or the monotonic clock advanced but the
        // process was frozen). Either way wall-clock will have leapt far
        // ahead of the scheduled interval. Reset the idle counter so the
        // user gets the full configured grace window after wake instead
        // of being suspended within seconds because the SSH client
        // connections all dropped during the host's sleep.
        let now = SystemTime::now();
        if looks_like_host_wake(last_tick_wall, now, PROBE_INTERVAL) {
            info!(
                vm = vm_name,
                "host wake detected (long gap between ticks); resetting idle timer"
            );
            idle_secs = 0;
            last_tick_wall = now;
            continue;
        }
        last_tick_wall = now;

        // Status guard: only count idle while the VM is Running.
        // Anything else (Configuring, Stopped, Suspended, Broken) resets
        // the timer; the cleanup paths are responsible for actually
        // killing the watcher when the VM transitions to a non-running
        // status — this is just defense in depth.
        match inst.reconcile_status().await {
            Ok(Status::Running) => {}
            Ok(other) => {
                debug!(vm = vm_name, status = %other, "VM not running; resetting idle timer");
                idle_secs = 0;
                continue;
            }
            Err(e) => {
                warn!(vm = vm_name, error = ?e, "reconcile_status failed; treating as unknown");
                continue;
            }
        }

        // Probe.
        match probe(&inst, &user).await {
            Ok((who_count, loadavg)) => match evaluate(who_count, loadavg, load_threshold) {
                Activity::Active => {
                    // Log every active tick, not just when idle_secs > 0.
                    // The old gate meant a VM that was active from the very
                    // first probe logged nothing at all — which is exactly
                    // how a stderr-polluted `who` count silently kept a VM
                    // "active" forever with no trace of why.
                    debug!(
                        vm = vm_name,
                        who = who_count,
                        load = loadavg,
                        idle_secs,
                        "tick active"
                    );
                    idle_secs = 0;
                }
                Activity::Idle => {
                    idle_secs = idle_secs.saturating_add(PROBE_INTERVAL.as_secs());
                    debug!(
                        vm = vm_name,
                        who = who_count,
                        load = loadavg,
                        idle_secs,
                        threshold_secs,
                        "tick idle"
                    );
                }
            },
            Err(e) => {
                debug!(vm = vm_name, error = ?e, "idle probe failed; treating as unknown");
                continue;
            }
        }

        if idle_secs >= threshold_secs {
            info!(
                vm = vm_name,
                minutes = threshold_minutes,
                "VM idle past threshold; suspending"
            );
            // Remove our pid file before triggering suspend so the
            // cleanup path inside `vm::suspend` doesn't SIGTERM us
            // mid-savevm. If suspend fails, we recreate the file below.
            let _ = tokio::fs::remove_file(inst.idle_watcher_pid_path()).await;
            match vm::suspend(vm_name).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    warn!(vm = vm_name, error = ?e, "auto-suspend failed; will retry next tick");
                    let _ = write_pid_file(&inst).await;
                    idle_secs = 0;
                }
            }
        }
    }

    // Signalled: best-effort pid-file cleanup so a future `agv start` doesn't
    // see a stale pointer.
    let _ = tokio::fs::remove_file(inst.idle_watcher_pid_path()).await;
    Ok(())
}

/// One probe: SSH in, read `who` and `/proc/loadavg`, return
/// `(interactive_session_count, 5-min load average)`.
///
/// Uses [`ssh::run_cmd_stdout`], not `run_cmd`: both outputs are parsed,
/// so stderr chatter (e.g. a guest `setlocale` warning when the client
/// forwards `LC_*`) must be kept out — otherwise it lands in the `who`
/// result and makes an idle VM look active forever.
async fn probe(inst: &Instance, user: &str) -> anyhow::Result<(u32, f32)> {
    let who_out = ssh::run_cmd_stdout(inst, user, &["who".to_string()]).await?;
    let who_count = count_interactive_sessions(&who_out);
    let loadavg_out = ssh::run_cmd_stdout(
        inst,
        user,
        &["cat".to_string(), "/proc/loadavg".to_string()],
    )
    .await?;
    let loadavg = parse_loadavg_5m(&loadavg_out)?;
    Ok((who_count, loadavg))
}

/// Count interactive sessions in `who` output.
///
/// A line counts only when one of its fields (after the user name) is a
/// tty device — see [`is_tty`]. That is the shape a real interactive
/// session has: `ssh` and `ssh -tt` allocate a PTY, which is recorded as
/// the utmp line.
///
/// The tty requirement exists because newer OpenSSH (Debian 13+ ships
/// 10.x) writes utmp entries for machine sessions too — the provisioning
/// commands, file copies, and this watcher's own probes agv runs over
/// SSH — and those entries can outlive their connection, showing up in
/// `who` with no tty forever after. Counting them would make a genuinely
/// idle VM look active and block auto-suspend permanently. On older
/// OpenSSH (Debian 12, 9.x) such entries never appear, so the filter is
/// inert there.
#[must_use]
pub fn count_interactive_sessions(who_output: &str) -> u32 {
    who_output
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|l| l.split_whitespace().skip(1).any(is_tty))
        .count()
        .try_into()
        .unwrap_or(u32::MAX)
}

/// Whether a `who` output field is a tty device — the interactive-session
/// marker.
///
/// Matches exactly the devices logins use, not a bare prefix: `console`,
/// `pts/<digits>` (PTYs — SSH, `ssh -tt`), and the tty families
/// (`tty<digits>` virtual consoles, `ttyS<n>` / `ttyAMA<n>` serial
/// consoles, `ttyACM<n>` / `ttyUSB<n>` serial adapters). util-linux wraps
/// the free-text client host in `(...)`, so a hostname can't normally
/// land here anyway — but the exact match keeps a `who` variant that
/// prints bare hostnames from letting something like `tty-host` fake a
/// session.
fn is_tty(field: &str) -> bool {
    if field == "console" {
        return true;
    }
    if let Some(n) = field.strip_prefix("pts/")
        && !n.is_empty()
        && n.bytes().all(|b| b.is_ascii_digit())
    {
        return true;
    }
    let Some(rest) = field.strip_prefix("tty") else {
        return false;
    };
    // Split the device family from its unit number at the first digit
    // (split_at keeps the digit, unlike split_once which drops it):
    // tty3 → ("", "3"), ttyS0 → ("S", "0"), ttyAMA0 → ("AMA", "0").
    let idx = rest
        .find(|c: char| c.is_ascii_digit())
        .unwrap_or(rest.len());
    let (name, num) = rest.split_at(idx);
    !num.is_empty()
        && num.bytes().all(|b| b.is_ascii_digit())
        && matches!(name, "" | "S" | "AMA" | "ACM" | "USB")
}

/// Extract the 5-minute load average from a `/proc/loadavg` line.
fn parse_loadavg_5m(s: &str) -> anyhow::Result<f32> {
    let trimmed = s.trim();
    let mut iter = trimmed.split_whitespace();
    let _one_min = iter.next().context("loadavg missing 1-min field")?;
    let five_min = iter.next().context("loadavg missing 5-min field")?;
    five_min
        .parse::<f32>()
        .with_context(|| format!("invalid 5-min loadavg: {five_min}"))
}

fn read_user(inst: &Instance) -> anyhow::Result<String> {
    let cfg = crate::config::load_resolved(&inst.config_path())?;
    Ok(cfg.user)
}

async fn write_pid_file(inst: &Instance) -> anyhow::Result<()> {
    let path = inst.idle_watcher_pid_path();
    tokio::fs::write(&path, std::process::id().to_string())
        .await
        .with_context(|| format!("failed to write {}", path.display()))
}

/// Spawn a detached idle-watcher process for the given VM.
///
/// Mirrors [`crate::vm::forwarding`]'s `spawn_supervisor`: stdio is
/// silenced, the child gets its own process group so cleanup paths can
/// SIGTERM the group, and the parent forgets the handle so the OS reaps
/// the eventual zombie.
///
/// If a stale watcher pid file exists with a still-live process, the old
/// watcher is killed first. Best-effort throughout — failures are logged
/// but don't propagate, since the VM is already up by the time we get
/// here.
pub async fn spawn(name: &str, threshold_minutes: u32, load_threshold: f32) {
    let Ok(inst) = Instance::open(name) else {
        warn!(vm = name, "idle_watcher::spawn: failed to open instance");
        return;
    };
    stop(&inst).await;

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            warn!(vm = name, error = %format!("{e:#}"), "could not locate agv binary; skipping idle watcher");
            return;
        }
    };
    // Redirect the watcher's stdout AND stderr to a per-instance log file so
    // its tracing output isn't lost to /dev/null. Both streams are captured
    // because `tracing_subscriber::fmt()` writes to stdout by default — a
    // stderr-only redirect leaves the log empty. The watcher has no
    // user-facing stdout of its own, so this is purely its log. Best-effort:
    // fall back to /dev/null if the file can't be opened, never fatal. Re-run
    // with `RUST_LOG=agv=debug` to capture the per-tick probe/idle lines.
    let (log_out, log_err) = match std::fs::File::create(inst.idle_watcher_log_path()) {
        Ok(file) => {
            let err = file
                .try_clone()
                .map_or_else(|_| Stdio::null(), Stdio::from);
            (Stdio::from(file), err)
        }
        Err(_) => (Stdio::null(), Stdio::null()),
    };

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("__idle-watcher")
        .arg(name)
        .arg(threshold_minutes.to_string())
        .arg(load_threshold.to_string())
        .stdin(Stdio::null())
        .stdout(log_out)
        .stderr(log_err);
    cmd.process_group(0);

    match cmd.spawn() {
        Ok(child) => {
            // The watcher writes its own pid file at startup. We forget the
            // handle so the OS reaps the eventual zombie when the watcher
            // exits — same idiom as forwarding::spawn_supervisor.
            std::mem::forget(child);
            info!(
                vm = name,
                minutes = threshold_minutes,
                threshold = load_threshold,
                "idle watcher spawned"
            );
        }
        Err(e) => {
            warn!(vm = name, error = %format!("{e:#}"), "failed to spawn idle watcher");
        }
    }
}

/// Stop the idle watcher for a VM (best-effort).
///
/// Reads the pid file, sends SIGTERM to the supervisor's process group,
/// and removes the pid file. Tolerates a missing file or a stale PID —
/// this runs from cleanup paths (stop, suspend, destroy) where the VM
/// may already be partway gone.
pub async fn stop(inst: &Instance) {
    let path = inst.idle_watcher_pid_path();
    let Ok(contents) = tokio::fs::read_to_string(&path).await else {
        return;
    };
    if let Ok(pid) = contents.trim().parse::<u32>() {
        forward::kill_supervisor(pid);
    }
    let _ = tokio::fs::remove_file(&path).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_when_no_session_and_low_load() {
        assert_eq!(evaluate(0, 0.05, 0.2), Activity::Idle);
    }

    #[test]
    fn active_when_session_present() {
        assert_eq!(evaluate(1, 0.0, 0.2), Activity::Active);
    }

    #[test]
    fn active_when_load_above_threshold() {
        assert_eq!(evaluate(0, 0.5, 0.2), Activity::Active);
    }

    #[test]
    fn active_when_load_at_threshold() {
        assert_eq!(evaluate(0, 0.2, 0.2), Activity::Active);
    }

    #[test]
    fn idle_with_zero_threshold_is_unreachable() {
        // Threshold 0 means "any nonzero load is active". A truly zero
        // load (0.0) is still idle by the strict-less-than rule.
        assert_eq!(evaluate(0, 0.0, 0.0), Activity::Active);
        assert_eq!(evaluate(0, 0.000_001, 0.0), Activity::Active);
    }

    #[test]
    fn parse_loadavg_extracts_five_min() {
        let s = "0.10 0.45 0.30 1/123 4567\n";
        let got = parse_loadavg_5m(s).unwrap();
        assert!((got - 0.45).abs() < 1e-6, "expected ~0.45, got {got}");
    }

    #[test]
    fn who_count_counts_pty_sessions() {
        // Debian 12-era format: user, tty, timestamp, host.
        let s = "agent    pts/0        2026-10-20 20:27 (10.0.2.2)\n";
        assert_eq!(count_interactive_sessions(s), 1);
    }

    #[test]
    fn who_count_counts_pty_sessions_openssh_10_format() {
        // Debian 13 / OpenSSH 10: interactive lines carry an extra
        // "sshd" token before the tty.
        let s = "agent    sshd pts/0   2026-10-20 20:27 (10.0.2.2)\r\n";
        assert_eq!(count_interactive_sessions(s), 1);
    }

    #[test]
    fn who_count_ignores_leftover_machine_session_entries() {
        // OpenSSH 10 leaves utmp entries for ended machine sessions
        // (agv's provisioning/probe connections) with no tty. They must
        // not count as activity.
        let s = "agent    sshd         2026-10-20 20:22 (10.0.2.2)\n";
        assert_eq!(count_interactive_sessions(s), 0);
    }

    #[test]
    fn who_count_ignores_ttyless_line_from_who_a() {
        // `who -a` prints the tty column as "?" for such entries.
        let s = "agent    ? sshd 2026-10-20 20:22 ? 912 (10.0.2.2)\n";
        assert_eq!(count_interactive_sessions(s), 0);
    }

    #[test]
    fn who_count_mixed_real_and_leftover_entries() {
        let s = concat!(
            "agent    sshd         2026-10-20 20:22 (10.0.2.2)\n",
            "agent    sshd pts/0   2026-10-20 20:27 (10.0.2.2)\n",
        );
        assert_eq!(count_interactive_sessions(s), 1);
    }

    #[test]
    fn who_count_counts_console_and_serial_logins() {
        let s = "agent    tty1         2026-10-20 20:27\nagent    ttyS0        2026-10-20 20:28\n";
        assert_eq!(count_interactive_sessions(s), 2);
    }

    #[test]
    fn who_count_empty_output_is_zero() {
        assert_eq!(count_interactive_sessions(""), 0);
        assert_eq!(count_interactive_sessions("\n\n"), 0);
    }

    #[test]
    fn tty_match_accepts_login_devices() {
        for f in [
            "console",
            "pts/0",
            "pts/12",
            "tty1",
            "tty12",
            "ttyS0",
            "ttyAMA0",
            "ttyACM0",
            "ttyUSB0",
        ] {
            assert!(is_tty(f), "{f} should match a tty device");
        }
    }

    #[test]
    fn tty_match_rejects_non_devices() {
        // Bare `tty` prefix matches would let a bare-hostname `who`
        // variant fake an interactive session.
        for f in [
            "sshd",
            "tty",
            "ttyhost",
            "tty1.example.com",
            "(10.0.2.2)",
            "10.0.2.2",
            "?",
            "2026-10-20",
        ] {
            assert!(!is_tty(f), "{f} should not match a tty device");
        }
    }

    #[test]
    fn parse_loadavg_rejects_garbage() {
        assert!(parse_loadavg_5m("").is_err());
        assert!(parse_loadavg_5m("0.10").is_err());
        assert!(parse_loadavg_5m("0.10 abc 0.30").is_err());
    }

    #[test]
    fn host_wake_not_detected_at_normal_interval() {
        let prev = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let now = prev + Duration::from_secs(60);
        assert!(!looks_like_host_wake(prev, now, Duration::from_secs(60)));
    }

    #[test]
    fn host_wake_not_detected_at_slight_overshoot() {
        // Tokio sleeps can fire a few ms late under load; a 1.5x interval
        // is well within normal jitter and must not trigger a reset.
        let prev = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let now = prev + Duration::from_secs(90);
        assert!(!looks_like_host_wake(prev, now, Duration::from_secs(60)));
    }

    #[test]
    fn host_wake_detected_for_large_gap() {
        // 1 hour of "sleep" while the watcher expected 60s.
        let prev = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let now = prev + Duration::from_secs(3600);
        assert!(looks_like_host_wake(prev, now, Duration::from_secs(60)));
    }

    #[test]
    fn host_wake_not_detected_on_backward_jump() {
        // NTP step backward / user manually adjusted the clock — wall time
        // moved into the past. Don't infer a host suspend from this.
        let prev = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let now = prev - Duration::from_secs(30);
        assert!(!looks_like_host_wake(prev, now, Duration::from_secs(60)));
    }
}
