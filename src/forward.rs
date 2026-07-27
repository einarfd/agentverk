//! Port forwarding spec types and state tracking.
//!
//! A forward is a mapping of a host port to a guest port. TCP is implicit:
//! forwards are tunneled via `ssh -L`, which is TCP-only. The specs here
//! are used by both the declarative config (`forwards = [...]` in
//! `agv.toml`) and the runtime CLI (`agv forward`).

use std::fmt;
use std::net::IpAddr;
use std::path::Path;
use std::str::FromStr;

use anyhow::{bail, Context as _};
use serde::{Deserialize, Serialize};

/// Where a forward's host-side listener binds.
///
/// A `ForwardSpec` with an empty `binds` list means loopback — `ssh -L`
/// with no bind address, i.e. `127.0.0.1` only, the safe default. Any
/// explicit target exposes the forwarded port more widely; see the
/// security note on [`ForwardSpec`].
///
/// Serialized as a plain string in TOML/JSON: an IP literal, or `*` for
/// [`BindTarget::All`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindTarget {
    /// A specific host address (IPv4 or IPv6), e.g. a tailnet IP.
    Addr(IpAddr),
    /// All interfaces, both address families — ssh's `*` bind address.
    All,
}

impl BindTarget {
    /// The address as it appears before a `:port` — IPv6 literals bracketed
    /// (`[::1]`), `*` and IPv4 as-is. Used for both `ssh -L`'s bind slot and
    /// `host:port` display (`agv inspect`).
    #[must_use]
    pub(crate) fn host_addr(self) -> String {
        match self {
            Self::Addr(IpAddr::V6(a)) => format!("[{a}]"),
            Self::Addr(ip) => ip.to_string(),
            Self::All => "*".to_string(),
        }
    }

    /// Whether this exposes the port beyond loopback — drives the security
    /// warning and the `GatewayPorts` toggle.
    #[must_use]
    pub fn is_non_loopback(self) -> bool {
        match self {
            Self::Addr(ip) => !ip.is_loopback(),
            Self::All => true,
        }
    }
}

impl fmt::Display for BindTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Addr(ip) => write!(f, "{ip}"),
            Self::All => write!(f, "*"),
        }
    }
}

impl FromStr for BindTarget {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let t = s.trim();
        if t == "*" {
            return Ok(Self::All);
        }
        let ip: IpAddr = t.parse().with_context(|| {
            format!("invalid bind address '{s}' — expected an IPv4/IPv6 address or '*'")
        })?;
        Ok(Self::Addr(ip))
    }
}

// Serde as a string ("*", "0.0.0.0", "::1", …) so the `[[forwards]]` table
// `bind` field and the JSON projection are plain strings.
impl TryFrom<String> for BindTarget {
    type Error = anyhow::Error;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}
impl From<BindTarget> for String {
    fn from(b: BindTarget) -> Self {
        b.to_string()
    }
}
impl Serialize for BindTarget {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}
impl<'de> Deserialize<'de> for BindTarget {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// A single forward specification: a host↔guest port mapping plus the
/// host addresses to bind.
///
/// If `guest` is omitted (string form), it defaults to the same value as
/// `host`. `binds` empty means loopback (`127.0.0.1`) — the default. Binds
/// are declared via the `[[forwards]]` table `bind` field (scalar or list),
/// a string spec's `@BIND` suffixes (`"8642@0.0.0.0"`, `"8642@10.0.0.1@::1"`),
/// or `agv forward --bind`. Non-loopback binds punch through agv's usual "the
/// SSH tunnel is the auth boundary" model, so they warn at apply time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardSpec {
    pub host: u16,
    pub guest: u16,
    pub binds: Vec<BindTarget>,
}

impl ForwardSpec {
    #[must_use]
    pub fn new(host: u16, guest: u16) -> Self {
        Self {
            host,
            guest,
            binds: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_binds(host: u16, guest: u16, binds: Vec<BindTarget>) -> Self {
        Self { host, guest, binds }
    }

    /// Render the port mapping as the short `host[:guest]` string. Note this
    /// deliberately omits `binds` — the bare-string form can't carry them
    /// (IPv6 literals collide with the `host:guest` colon). Use it for the
    /// port-only round-trip, not to fully describe a bound forward.
    #[must_use]
    pub fn to_short_string(&self) -> String {
        if self.host == self.guest {
            self.host.to_string()
        } else {
            format!("{}:{}", self.host, self.guest)
        }
    }

    /// Whether any bind exposes the port beyond loopback.
    #[must_use]
    pub fn has_non_loopback_bind(&self) -> bool {
        self.binds.iter().any(|b| b.is_non_loopback())
    }

    /// The `-L` argument value(s) for `ssh`, one per bind address. With no
    /// binds, a single loopback forward (ssh's default). The middle
    /// `localhost` is the guest-side destination resolved by sshd inside
    /// the guest.
    #[must_use]
    pub fn ssh_forward_args(&self) -> Vec<String> {
        if self.binds.is_empty() {
            return vec![format!("{}:localhost:{}", self.host, self.guest)];
        }
        self.binds
            .iter()
            .map(|b| format!("{}:{}:localhost:{}", b.host_addr(), self.host, self.guest))
            .collect()
    }
}

impl fmt::Display for ForwardSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_short_string())?;
        if !self.binds.is_empty() {
            let binds: Vec<String> = self.binds.iter().map(ToString::to_string).collect();
            write!(f, " (bind: {})", binds.join(", "))?;
        }
        Ok(())
    }
}

impl FromStr for ForwardSpec {
    type Err = anyhow::Error;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let s = raw.trim();
        if s.is_empty() {
            bail!("empty forward spec");
        }

        // Split off any `@BIND` suffixes. The mapping is everything before
        // the first `@`; each following segment is one bind address (an IP
        // or `*`). `@` can't appear in a port or an IP, so the split is
        // unambiguous even for IPv6 binds (whose colons all sit after `@`).
        let mut segments = s.split('@');
        let mapping = segments.next().unwrap_or(s).trim();

        // `/proto` suffixes were accepted in early versions of agv but never
        // did anything — every tunnel was TCP regardless. Reject with a
        // clear message so users with legacy configs know to remove it.
        if let Some((_, proto_part)) = mapping.split_once('/') {
            bail!(
                "forward spec '{raw}' has a '/{proto_part}' protocol suffix, \
                 which is no longer accepted — TCP is implicit (the underlying \
                 `ssh -L` tunnel is TCP-only). Drop the suffix: '{}'",
                mapping.split_once('/').map_or(mapping, |(p, _)| p)
            );
        }

        // Parse host[:guest].
        let (host_str, guest_str) = match mapping.split_once(':') {
            Some((h, g)) => (h, g),
            None => (mapping, mapping),
        };

        let host: u16 = parse_port(host_str).with_context(|| format!("host port in '{raw}'"))?;
        let guest: u16 = parse_port(guest_str).with_context(|| format!("guest port in '{raw}'"))?;

        let mut binds = Vec::new();
        for segment in segments {
            let b = segment.trim();
            if b.is_empty() {
                bail!("empty bind address after '@' in '{raw}'");
            }
            binds.push(
                b.parse::<BindTarget>()
                    .with_context(|| format!("bind address in '{raw}'"))?,
            );
        }

        Ok(Self::with_binds(host, guest, binds))
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

/// Parse a list of forward spec strings, reporting the first error. Each is
/// `HOST[:GUEST][@BIND]...`; every `@BIND` suffix (an IP or `*`) adds one
/// bind address, so `8642@10.0.0.1@::1` binds both.
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

/// The `(bind-address, host-port)` keys a spec occupies, for duplicate
/// detection. A loopback (default) forward is keyed as `127.0.0.1` — the
/// address `ssh -L` binds by default — so it collides with an explicit
/// `bind = "127.0.0.1"` on the same port. Explicit binds are keyed by their
/// string form (an IP or `*`).
#[must_use]
pub fn bind_keys(spec: &ForwardSpec) -> Vec<(String, u16)> {
    if spec.binds.is_empty() {
        vec![("127.0.0.1".to_string(), spec.host)]
    } else {
        spec.binds
            .iter()
            .map(|b| (b.to_string(), spec.host))
            .collect()
    }
}

/// Print a stderr warning for every forward that binds past loopback.
///
/// A non-loopback bind exposes the guest service beyond `127.0.0.1`, dropping
/// agv's "the SSH tunnel is the only gate" guarantee. Always written to stderr
/// (never suppressed by `--quiet`) because it's a security notice, not status
/// output. Shared by the ad-hoc `agv forward` path and the start/resume path
/// so the wording and channel stay identical.
pub fn warn_non_loopback_binds(specs: &[ForwardSpec]) {
    for spec in specs {
        if !spec.has_non_loopback_bind() {
            continue;
        }
        let addrs: Vec<String> = spec
            .binds
            .iter()
            .filter(|b| b.is_non_loopback())
            .map(ToString::to_string)
            .collect();
        eprintln!(
            "  ⚠ host port {} is bound to {} — reachable beyond localhost; \
             anything that can reach that address reaches the guest service \
             (the SSH tunnel is no longer the only gate).",
            spec.host,
            addrs.join(", "),
        );
    }
}

/// Validate that no two forwards would bind the same `(address, host port)`.
///
/// Keys on the bind address string (loopback default rendered as
/// `localhost`), so `8642` on `127.0.0.1` and `8642` on `192.168.1.5` are
/// allowed but the same address twice is not. Genuine wildcard overlaps
/// (a loopback forward plus a `0.0.0.0` one on the same port) aren't
/// second-guessed here — they surface as the supervisor's ssh bind error.
pub fn validate_unique(specs: &[ForwardSpec]) -> anyhow::Result<()> {
    let mut seen: std::collections::HashSet<(String, u16)> = std::collections::HashSet::new();
    for spec in specs {
        for key in bind_keys(spec) {
            if !seen.insert(key.clone()) {
                bail!(
                    "duplicate forward for host port {} on bind address {} in list",
                    spec.host,
                    key.0
                );
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveForward {
    pub host: u16,
    pub guest: u16,
    /// Host bind addresses; empty = loopback. `#[serde(default)]` so
    /// pre-bind `forwards.toml` files (which had no `binds` key) still load.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub binds: Vec<BindTarget>,
    pub origin: Origin,
    pub pid: u32,
}

impl ActiveForward {
    #[must_use]
    pub fn new(spec: ForwardSpec, origin: Origin, pid: u32) -> Self {
        Self {
            host: spec.host,
            guest: spec.guest,
            binds: spec.binds,
            origin,
            pid,
        }
    }

    #[must_use]
    pub fn spec(&self) -> ForwardSpec {
        ForwardSpec::with_binds(self.host, self.guest, self.binds.clone())
    }
}

/// JSON projection of `ActiveForward` for `agv forward --list --json`
/// and the `forwards` field of `VmStateReport`.
///
/// Drops `pid` (an internal supervisor process detail that's not part of
/// the agent-facing contract) but exposes `alive`, computed from that
/// PID at conversion time. `--list` always emits `alive: true` because
/// it sweeps dead entries before serializing; `inspect` doesn't sweep,
/// so a stale entry in `forwards.toml` shows up with `alive: false` —
/// useful diagnostically. Stable across the 0.x series — additions
/// OK, removals/renames need a major bump.
#[derive(Debug, Clone, Serialize)]
pub struct ForwardJson {
    pub host: u16,
    pub guest: u16,
    /// Host bind addresses as strings (an IP or `*`). Empty array = the
    /// default loopback bind. Additive field — agents that predate it keep
    /// working.
    pub binds: Vec<String>,
    pub origin: Origin,
    pub alive: bool,
}

impl From<ActiveForward> for ForwardJson {
    fn from(a: ActiveForward) -> Self {
        Self {
            host: a.host,
            guest: a.guest,
            binds: a.binds.iter().map(ToString::to_string).collect(),
            origin: a.origin,
            alive: is_alive(a.pid),
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

/// Check whether a process with this PID is still alive.
///
/// Used by `sweep_dead` to drop forward entries whose supervisor died,
/// and by `inspect`/`VmStateReport` to surface the per-forward and
/// idle-watcher liveness flags. `rustix::process::test_kill_process`
/// returns `Ok(())` when signal 0 to the PID would have been
/// deliverable — i.e. the process exists; we don't actually send a
/// signal.
#[must_use]
pub fn is_alive(pid: u32) -> bool {
    pid_from_u32(pid).is_some_and(|p| rustix::process::test_kill_process(p).is_ok())
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
            binds: vec![],
            origin: Origin::Config,
            alive: true,
        };
        let json = serde_json::to_value(entry).unwrap();
        let obj = json.as_object().expect("ForwardJson must serialize as an object");
        let actual: std::collections::BTreeSet<&str> =
            obj.keys().map(String::as_str).collect();
        let expected: std::collections::BTreeSet<&str> =
            ["alive", "binds", "guest", "host", "origin"].into_iter().collect();
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
            let entry = ForwardJson { host: 1, guest: 1, binds: vec![], origin, alive: true };
            let json = serde_json::to_value(entry).unwrap();
            assert_eq!(
                json.get("origin"),
                Some(&serde_json::Value::String(expected.to_string())),
            );
        }
    }

    // --- BindTarget parsing / rendering ---

    // --- string form with an @BIND suffix ---

    #[test]
    fn parses_at_bind_ipv4() {
        let s: ForwardSpec = "8642@0.0.0.0".parse().unwrap();
        assert_eq!(s, ForwardSpec::with_binds(8642, 8642, vec!["0.0.0.0".parse().unwrap()]));
    }

    #[test]
    fn parses_host_guest_with_at_bind() {
        let s: ForwardSpec = "8080:80@192.168.1.5".parse().unwrap();
        assert_eq!(
            s,
            ForwardSpec::with_binds(8080, 80, vec!["192.168.1.5".parse().unwrap()])
        );
    }

    #[test]
    fn parses_at_bind_ipv6_unambiguously() {
        // The IPv6 literal's colons sit after `@`, so they don't collide with
        // the host:guest separator.
        let s: ForwardSpec = "8642@2001:db8::5".parse().unwrap();
        assert_eq!(
            s,
            ForwardSpec::with_binds(8642, 8642, vec!["2001:db8::5".parse().unwrap()])
        );
    }

    #[test]
    fn parses_at_bind_star() {
        let s: ForwardSpec = "8642@*".parse().unwrap();
        assert_eq!(s.binds, vec![BindTarget::All]);
    }

    #[test]
    fn rejects_bad_and_empty_at_bind() {
        assert!("8642@not-an-ip".parse::<ForwardSpec>().is_err());
        assert!("8642@".parse::<ForwardSpec>().is_err());
        let err = "8642@nope".parse::<ForwardSpec>().unwrap_err();
        assert!(format!("{err:#}").contains("bind address"), "got: {err:#}");
    }

    #[test]
    fn parses_repeated_at_binds() {
        let s: ForwardSpec = "8642@10.0.0.1@::1".parse().unwrap();
        assert_eq!(s.host, 8642);
        assert_eq!(s.guest, 8642);
        assert_eq!(
            s.binds,
            vec![
                "10.0.0.1".parse::<BindTarget>().unwrap(),
                "::1".parse::<BindTarget>().unwrap(),
            ]
        );
    }

    #[test]
    fn repeated_at_binds_combine_with_host_guest_mapping() {
        let s: ForwardSpec = "8080:80@0.0.0.0@192.168.1.5".parse().unwrap();
        assert_eq!((s.host, s.guest), (8080, 80));
        assert_eq!(s.binds.len(), 2);
        assert!(s.has_non_loopback_bind());
    }

    #[test]
    fn rejects_empty_segment_among_repeated_at_binds() {
        // A stray `@@` or trailing `@` must not silently yield fewer binds
        // than the user wrote.
        assert!("8642@10.0.0.1@".parse::<ForwardSpec>().is_err());
        assert!("8642@@::1".parse::<ForwardSpec>().is_err());
    }

    #[test]
    fn repeated_at_binds_emit_one_ssh_arg_each() {
        let s: ForwardSpec = "8642@10.0.0.1@::1".parse().unwrap();
        assert_eq!(
            s.ssh_forward_args(),
            vec![
                "10.0.0.1:8642:localhost:8642".to_string(),
                "[::1]:8642:localhost:8642".to_string(),
            ]
        );
    }

    #[test]
    fn duplicate_binds_within_one_spec_are_rejected_by_validate_unique() {
        let s: ForwardSpec = "8642@10.0.0.1@10.0.0.1".parse().unwrap();
        let err = validate_unique(&[s]).unwrap_err();
        assert!(format!("{err:#}").contains("duplicate forward"), "got: {err:#}");
    }

    #[test]
    fn at_bind_parses_through_parse_specs() {
        let specs = parse_specs(["8642@0.0.0.0", "5433:5432", "3000@*"]).unwrap();
        assert_eq!(specs.len(), 3);
        assert_eq!(specs[0].binds, vec!["0.0.0.0".parse().unwrap()]);
        assert!(specs[1].binds.is_empty());
        assert_eq!(specs[2].binds, vec![BindTarget::All]);
    }

    #[test]
    fn bind_target_parses_ipv4_ipv6_and_star() {
        assert_eq!("0.0.0.0".parse::<BindTarget>().unwrap(), BindTarget::Addr("0.0.0.0".parse().unwrap()));
        assert_eq!("::1".parse::<BindTarget>().unwrap(), BindTarget::Addr("::1".parse().unwrap()));
        assert_eq!("*".parse::<BindTarget>().unwrap(), BindTarget::All);
        assert!("not-an-ip".parse::<BindTarget>().is_err());
        assert!("tailscale0".parse::<BindTarget>().is_err()); // interface names rejected
    }

    #[test]
    fn bind_target_non_loopback_detection() {
        assert!(!"127.0.0.1".parse::<BindTarget>().unwrap().is_non_loopback());
        assert!(!"::1".parse::<BindTarget>().unwrap().is_non_loopback());
        assert!("0.0.0.0".parse::<BindTarget>().unwrap().is_non_loopback());
        assert!("192.168.1.5".parse::<BindTarget>().unwrap().is_non_loopback());
        assert!(BindTarget::All.is_non_loopback());
    }

    #[test]
    fn ssh_forward_args_loopback_default() {
        let spec = ForwardSpec::new(8642, 8642);
        assert_eq!(spec.ssh_forward_args(), vec!["8642:localhost:8642"]);
    }

    #[test]
    fn ssh_forward_args_brackets_ipv6_and_emits_one_per_bind() {
        let spec = ForwardSpec::with_binds(
            8642,
            80,
            vec![
                "192.168.1.5".parse().unwrap(),
                "2001:db8::5".parse().unwrap(),
                "*".parse().unwrap(),
            ],
        );
        assert_eq!(
            spec.ssh_forward_args(),
            vec![
                "192.168.1.5:8642:localhost:80",
                "[2001:db8::5]:8642:localhost:80",
                "*:8642:localhost:80",
            ]
        );
    }

    #[test]
    fn validate_unique_allows_same_port_on_distinct_binds() {
        let specs = vec![
            ForwardSpec::with_binds(8642, 8642, vec!["192.168.1.5".parse().unwrap()]),
            ForwardSpec::with_binds(8642, 8642, vec!["10.0.0.1".parse().unwrap()]),
        ];
        validate_unique(&specs).unwrap();
    }

    #[test]
    fn validate_unique_rejects_same_port_same_bind() {
        let specs = vec![
            ForwardSpec::with_binds(8642, 8642, vec!["192.168.1.5".parse().unwrap()]),
            ForwardSpec::with_binds(8642, 3000, vec!["192.168.1.5".parse().unwrap()]),
        ];
        let err = validate_unique(&specs).unwrap_err();
        assert!(format!("{err:#}").contains("192.168.1.5"));
    }

    #[test]
    fn validate_unique_rejects_duplicate_loopback_port() {
        // Two loopback (default) forwards on the same host port still clash.
        let specs = vec![ForwardSpec::new(8080, 8080), ForwardSpec::new(8080, 3000)];
        assert!(validate_unique(&specs).is_err());
    }

    #[test]
    fn validate_unique_default_bind_collides_with_explicit_loopback() {
        // A default (empty-binds) forward binds 127.0.0.1, so an explicit
        // `bind = "127.0.0.1"` on the same port is the same socket.
        let specs = vec![
            ForwardSpec::new(8080, 8080),
            ForwardSpec::with_binds(8080, 3000, vec!["127.0.0.1".parse().unwrap()]),
        ];
        assert!(validate_unique(&specs).is_err());
    }

    #[test]
    fn active_forward_roundtrips_binds() {
        let spec = ForwardSpec::with_binds(8642, 80, vec![BindTarget::All, "::1".parse().unwrap()]);
        let active = ActiveForward::new(spec.clone(), Origin::Adhoc, 42);
        assert_eq!(active.spec(), spec);
        assert_eq!(active.binds.len(), 2);
    }
}
