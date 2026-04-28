//! Port forwarding spec types and state tracking.
//!
//! A forward is a mapping of a host port to a guest port. TCP is implicit:
//! forwards are tunneled via `ssh -L`, which is TCP-only. The specs here
//! are used by both the declarative config (`forwards = [...]` in
//! `agv.toml`) and the runtime CLI (`agv forward`).

use std::fmt;
use std::path::Path;
use std::str::FromStr;

use anyhow::{bail, Context as _};
use serde::{Deserialize, Serialize};

/// A single forward specification: `host[:guest]`.
///
/// If `guest` is omitted, it defaults to the same value as `host`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardSpec {
    pub host: u16,
    pub guest: u16,
}

impl ForwardSpec {
    #[must_use]
    pub fn new(host: u16, guest: u16) -> Self {
        Self { host, guest }
    }

    /// Render as the short form suitable for CLI/config round-trip.
    #[must_use]
    pub fn to_short_string(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for ForwardSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host == self.guest {
            write!(f, "{}", self.host)
        } else {
            write!(f, "{}:{}", self.host, self.guest)
        }
    }
}

impl FromStr for ForwardSpec {
    type Err = anyhow::Error;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let s = raw.trim();
        if s.is_empty() {
            bail!("empty forward spec");
        }

        // `/proto` suffixes were accepted in early versions of agv but never
        // did anything — every tunnel was TCP regardless. Reject with a
        // clear message so users with legacy configs know to remove it.
        if let Some((_, proto_part)) = s.split_once('/') {
            bail!(
                "forward spec '{raw}' has a '/{proto_part}' protocol suffix, \
                 which is no longer accepted — TCP is implicit (the underlying \
                 `ssh -L` tunnel is TCP-only). Drop the suffix: '{}'",
                s.split_once('/').map_or(s, |(p, _)| p)
            );
        }

        // Parse host[:guest].
        let (host_str, guest_str) = match s.split_once(':') {
            Some((h, g)) => (h, g),
            None => (s, s),
        };

        let host: u16 = parse_port(host_str).with_context(|| format!("host port in '{raw}'"))?;
        let guest: u16 = parse_port(guest_str).with_context(|| format!("guest port in '{raw}'"))?;

        Ok(Self { host, guest })
    }
}

fn parse_port(s: &str) -> anyhow::Result<u16> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        bail!("port is empty");
    }
    let n: u16 = trimmed
        .parse()
        .with_context(|| format!("'{trimmed}' is not a valid port (0-65535)"))?;
    if n == 0 {
        bail!("port 0 is not allowed");
    }
    Ok(n)
}

/// Parse a list of forward spec strings, reporting the first error.
pub fn parse_specs<I, S>(raw: I) -> anyhow::Result<Vec<ForwardSpec>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = Vec::new();
    for item in raw {
        let spec: ForwardSpec = item.as_ref().parse()?;
        out.push(spec);
    }
    Ok(out)
}

/// Validate that a list of forward specs has no duplicate host ports.
///
/// Two forwards binding the same host port would conflict at runtime;
/// catching it up front gives a clearer error than letting the supervisor's
/// ssh fail.
pub fn validate_unique(specs: &[ForwardSpec]) -> anyhow::Result<()> {
    for (i, a) in specs.iter().enumerate() {
        for b in &specs[i + 1..] {
            if a.host == b.host {
                bail!("duplicate forward for host port {} in list", a.host);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Active forward state (persisted as `<instance>/forwards.toml`)
// ---------------------------------------------------------------------------

/// Where a forward originated — used to distinguish declarative config
/// entries from ad-hoc `agv forward` additions in `--list` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// Declared in `agv.toml` / `[[forward]]` / `forwards = [...]`.
    Config,
    /// Added at runtime via `agv forward`.
    Adhoc,
    /// Created by a mixin via `[auto_forwards.<name>]` — the host port was
    /// auto-allocated at VM start and written to `<instance>/<name>_port`.
    Auto,
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config => write!(f, "config"),
            Self::Adhoc => write!(f, "adhoc"),
            Self::Auto => write!(f, "auto"),
        }
    }
}

/// A forward currently active on a running VM.
///
/// Each active entry is backed by an agv-spawned supervisor process that
/// runs a respawn loop around `ssh -N -L`. The `pid` is the supervisor's
/// process group leader, so stopping the forward means group-killing `pid`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveForward {
    pub host: u16,
    pub guest: u16,
    pub origin: Origin,
    pub pid: u32,
}

impl ActiveForward {
    #[must_use]
    pub fn new(spec: ForwardSpec, origin: Origin, pid: u32) -> Self {
        Self {
            host: spec.host,
            guest: spec.guest,
            origin,
            pid,
        }
    }

    #[must_use]
    pub fn spec(&self) -> ForwardSpec {
        ForwardSpec {
            host: self.host,
            guest: self.guest,
        }
    }
}

/// JSON projection of `ActiveForward` for `agv forward --list --json`.
///
/// Drops `pid` (an internal supervisor process detail that's not part of
/// the agent-facing contract). Stable across the 0.x series — additions
/// OK, removals/renames need a major bump.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ForwardJson {
    pub host: u16,
    pub guest: u16,
    pub origin: Origin,
}

impl From<ActiveForward> for ForwardJson {
    fn from(a: ActiveForward) -> Self {
        Self {
            host: a.host,
            guest: a.guest,
            origin: a.origin,
        }
    }
}

/// Wrapper used for TOML (de)serialization of the state file.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ActiveForwardsFile {
    #[serde(default)]
    active: Vec<ActiveForward>,
}

/// Read the active-forwards state file, returning an empty vec if missing.
pub async fn read_active(path: &Path) -> anyhow::Result<Vec<ActiveForward>> {
    let contents = match tokio::fs::read_to_string(path).await {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let file: ActiveForwardsFile = toml::from_str(&contents)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(file.active)
}

/// Write the active-forwards state file, or remove it when the list is empty.
pub async fn write_active(path: &Path, active: &[ActiveForward]) -> anyhow::Result<()> {
    if active.is_empty() {
        match tokio::fs::remove_file(path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("failed to remove {}", path.display())),
        }
    } else {
        let file = ActiveForwardsFile {
            active: active.to_vec(),
        };
        let toml_str =
            toml::to_string_pretty(&file).context("failed to serialize forwards state")?;
        tokio::fs::write(path, toml_str)
            .await
            .with_context(|| format!("failed to write {}", path.display()))
    }
}

/// Remove the active-forwards state file if it exists.
pub async fn clear_active(path: &Path) -> anyhow::Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("failed to remove {}", path.display())),
    }
}

/// Convert a stored `u32` PID into the `rustix` PID newtype.
///
/// Returns `None` for 0 (which `rustix` rejects as the "calling process"
/// sentinel) or values outside the `i32` range. Centralises the two
/// fallible conversion steps every PID-using callsite would otherwise
/// repeat.
#[must_use]
pub fn pid_from_u32(pid: u32) -> Option<rustix::process::Pid> {
    rustix::process::Pid::from_raw(i32::try_from(pid).ok()?)
}

/// Send SIGTERM to a supervisor process group. Tolerates an already-dead PID.
///
/// The supervisor was spawned in its own process group, so signalling the
/// group reaches the supervisor and any in-flight `ssh` child it spawned.
/// Uses `rustix::process::kill_process_group` instead of shelling out to
/// `kill(1)`, which has subtly different argument-parsing rules between
/// Linux util-linux and macOS BSD `kill` for negative-PID arguments.
pub fn kill_supervisor(pid: u32) {
    let Some(p) = pid_from_u32(pid) else {
        return;
    };
    let _ = rustix::process::kill_process_group(p, rustix::process::Signal::TERM);
}

/// Best-effort: kill every supervisor listed in `path` and remove the file.
pub async fn kill_all_and_clear(path: &Path) {
    let Ok(active) = read_active(path).await else {
        return;
    };
    for entry in &active {
        kill_supervisor(entry.pid);
    }
    let _ = clear_active(path).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parses_single_port() {
        let s: ForwardSpec = "8080".parse().unwrap();
        assert_eq!(s, ForwardSpec::new(8080, 8080));
    }

    #[test]
    fn parses_host_guest() {
        let s: ForwardSpec = "8080:3000".parse().unwrap();
        assert_eq!(s, ForwardSpec::new(8080, 3000));
    }

    #[test]
    fn trims_whitespace() {
        let s: ForwardSpec = "  8080:3000  ".parse().unwrap();
        assert_eq!(s, ForwardSpec::new(8080, 3000));
    }

    #[test]
    fn rejects_empty() {
        assert!("".parse::<ForwardSpec>().is_err());
        assert!("   ".parse::<ForwardSpec>().is_err());
    }

    #[test]
    fn rejects_zero_port() {
        assert!("0".parse::<ForwardSpec>().is_err());
        assert!("8080:0".parse::<ForwardSpec>().is_err());
    }

    #[test]
    fn rejects_out_of_range() {
        assert!("70000".parse::<ForwardSpec>().is_err());
        assert!("-1".parse::<ForwardSpec>().is_err());
    }

    #[test]
    fn rejects_non_numeric() {
        assert!("abc".parse::<ForwardSpec>().is_err());
        assert!("80:xyz".parse::<ForwardSpec>().is_err());
    }

    #[test]
    fn rejects_missing_guest_with_colon() {
        assert!("80:".parse::<ForwardSpec>().is_err());
        assert!(":80".parse::<ForwardSpec>().is_err());
    }

    /// Legacy `/tcp` or `/udp` suffixes (supported by early versions but
    /// never functional for UDP) now fail at parse time with a clear
    /// message rather than silently tunneling TCP.
    #[test]
    fn rejects_proto_suffix_with_helpful_message() {
        for bad in ["53/udp", "80/tcp", "8080:3000/udp", "53/sctp"] {
            let err = bad.parse::<ForwardSpec>().unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("protocol suffix") && msg.contains("TCP"),
                "expected protocol-suffix error for {bad:?}, got: {msg}"
            );
        }
    }

    #[test]
    fn display_roundtrip_single_port() {
        let s = ForwardSpec::new(8080, 8080);
        assert_eq!(s.to_string(), "8080");
        assert_eq!(s.to_string().parse::<ForwardSpec>().unwrap(), s);
    }

    #[test]
    fn display_roundtrip_host_guest() {
        let s = ForwardSpec::new(8080, 3000);
        assert_eq!(s.to_string(), "8080:3000");
        assert_eq!(s.to_string().parse::<ForwardSpec>().unwrap(), s);
    }

    #[test]
    fn parse_specs_collects_all() {
        let raw = ["8080", "3000:5000", "53"];
        let specs = parse_specs(raw).unwrap();
        assert_eq!(specs.len(), 3);
        assert_eq!(specs[0], ForwardSpec::new(8080, 8080));
        assert_eq!(specs[1], ForwardSpec::new(3000, 5000));
        assert_eq!(specs[2], ForwardSpec::new(53, 53));
    }

    #[test]
    fn parse_specs_reports_first_error() {
        let raw = ["8080", "not-a-port"];
        let err = parse_specs(raw).unwrap_err();
        assert!(err.to_string().contains("not-a-port"));
    }

    #[test]
    fn validate_unique_accepts_distinct_host_ports() {
        let specs = vec![
            ForwardSpec::new(8080, 8080),
            ForwardSpec::new(8081, 8080),
            ForwardSpec::new(9000, 3000),
        ];
        validate_unique(&specs).unwrap();
    }

    #[test]
    fn validate_unique_rejects_duplicate_host_port() {
        let specs = vec![
            ForwardSpec::new(8080, 8080),
            ForwardSpec::new(8080, 3000),
        ];
        let err = validate_unique(&specs).unwrap_err();
        assert!(err.to_string().contains("8080"));
    }

    #[tokio::test]
    async fn active_forwards_empty_when_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("forwards.toml");
        let active = read_active(&path).await.unwrap();
        assert!(active.is_empty());
    }

    #[tokio::test]
    async fn active_forwards_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("forwards.toml");
        let original = vec![
            ActiveForward::new(ForwardSpec::new(8080, 8080), Origin::Config, 12345),
            ActiveForward::new(ForwardSpec::new(53, 53), Origin::Adhoc, 54321),
        ];
        write_active(&path, &original).await.unwrap();
        let loaded = read_active(&path).await.unwrap();
        assert_eq!(loaded, original);
    }

    #[tokio::test]
    async fn active_forwards_empty_write_removes_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("forwards.toml");
        // Write something first.
        write_active(
            &path,
            &[ActiveForward::new(
                ForwardSpec::new(8080, 8080),
                Origin::Config,
                12345,
            )],
        )
        .await
        .unwrap();
        assert!(path.exists());
        // Writing empty clears the file.
        write_active(&path, &[]).await.unwrap();
        assert!(!path.exists());
    }

    /// Spawn a long-sleeping child in its own process group so we can test
    /// `kill_supervisor` against a real PID without depending on agv itself.
    fn spawn_sleep() -> std::process::Child {
        use std::os::unix::process::CommandExt as _;
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("30")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        cmd.process_group(0);
        cmd.spawn().expect("failed to spawn sleep for test")
    }

    fn pid_alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    #[tokio::test]
    async fn kill_supervisor_terminates_alive_pid() {
        let mut child = spawn_sleep();
        let pid = child.id();
        assert!(pid_alive(pid), "sleep should be alive after spawn");
        kill_supervisor(pid);
        // Reap to avoid leaving a zombie; SIGTERM should make sleep exit.
        let status = tokio::task::spawn_blocking(move || child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(!status.success(), "sleep was killed, should not exit 0");
        assert!(!pid_alive(pid), "pid should be dead after kill");
    }

    #[tokio::test]
    async fn kill_supervisor_tolerates_dead_pid() {
        let mut child = spawn_sleep();
        let pid = child.id();
        // Kill and reap first so the PID is definitely free.
        kill_supervisor(pid);
        let _ = tokio::task::spawn_blocking(move || child.wait()).await;
        // A second kill against an already-dead PID must not panic.
        kill_supervisor(pid);
    }

    #[tokio::test]
    async fn kill_all_and_clear_kills_listed_pids() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("forwards.toml");

        let child_a = spawn_sleep();
        let child_b = spawn_sleep();
        let pid_a = child_a.id();
        let pid_b = child_b.id();
        let entries = vec![
            ActiveForward::new(ForwardSpec::new(8080, 8080), Origin::Adhoc, pid_a),
            ActiveForward::new(ForwardSpec::new(9090, 9090), Origin::Config, pid_b),
        ];
        write_active(&path, &entries).await.unwrap();

        kill_all_and_clear(&path).await;

        // File is gone.
        assert!(!path.exists(), "forwards.toml should be removed");
        // Both children should die — reap them so they don't linger.
        for mut child in [child_a, child_b] {
            let _ = tokio::task::spawn_blocking(move || child.wait()).await;
        }
        assert!(!pid_alive(pid_a));
        assert!(!pid_alive(pid_b));
    }

    #[tokio::test]
    async fn clear_active_is_idempotent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("forwards.toml");
        // Clearing a non-existent file is fine.
        clear_active(&path).await.unwrap();
        // And after writing.
        write_active(
            &path,
            &[ActiveForward::new(
                ForwardSpec::new(8080, 8080),
                Origin::Config,
                12345,
            )],
        )
        .await
        .unwrap();
        clear_active(&path).await.unwrap();
        assert!(!path.exists());
    }

    /// Schema pin for `agv forward --list --json` entries — drift in this
    /// shape is a major-version bump.
    #[test]
    fn forward_json_schema_pin() {
        let entry = ForwardJson {
            host: 8080,
            guest: 8080,
            origin: Origin::Config,
        };
        let json = serde_json::to_value(entry).unwrap();
        let obj = json.as_object().expect("ForwardJson must serialize as an object");
        let actual: std::collections::BTreeSet<&str> =
            obj.keys().map(String::as_str).collect();
        let expected: std::collections::BTreeSet<&str> =
            ["guest", "host", "origin"].into_iter().collect();
        assert_eq!(actual, expected, "ForwardJson keys drifted");
    }

    /// `Origin` round-trips as a lowercase string variant — agents
    /// pattern-match on it.
    #[test]
    fn forward_json_origin_serializes_lowercase() {
        let cases = [
            (Origin::Config, "config"),
            (Origin::Adhoc, "adhoc"),
            (Origin::Auto, "auto"),
        ];
        for (origin, expected) in cases {
            let entry = ForwardJson { host: 1, guest: 1, origin };
            let json = serde_json::to_value(entry).unwrap();
            assert_eq!(
                json.get("origin"),
                Some(&serde_json::Value::String(expected.to_string())),
            );
        }
    }
}
