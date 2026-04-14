//! Port forwarding spec types and state tracking.
//!
//! A forward is a mapping of a host port to a guest port over a protocol
//! (TCP or UDP). Forwards are applied to a running VM via QEMU's hostfwd
//! mechanism (see `vm::qemu::hostfwd_add`). The specs defined here are used
//! by both the declarative config (`forwards = [...]` in `agv.toml`) and the
//! runtime CLI (`agv forward`).

use std::fmt;
use std::path::Path;
use std::str::FromStr;

use anyhow::{bail, Context as _};
use serde::{Deserialize, Serialize};

/// Transport protocol for a forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Proto {
    #[default]
    Tcp,
    Udp,
}

impl fmt::Display for Proto {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tcp => write!(f, "tcp"),
            Self::Udp => write!(f, "udp"),
        }
    }
}

impl FromStr for Proto {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "tcp" => Ok(Self::Tcp),
            "udp" => Ok(Self::Udp),
            other => bail!("unknown protocol '{other}' — expected 'tcp' or 'udp'"),
        }
    }
}

/// A single forward specification: `host[:guest][/proto]`.
///
/// If `guest` is omitted, it defaults to the same value as `host`.
/// If `/proto` is omitted, it defaults to TCP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardSpec {
    pub host: u16,
    pub guest: u16,
    #[serde(default)]
    pub proto: Proto,
}

impl ForwardSpec {
    #[must_use]
    pub fn new(host: u16, guest: u16, proto: Proto) -> Self {
        Self { host, guest, proto }
    }

    /// Render as the short form suitable for CLI/config round-trip.
    #[must_use]
    pub fn to_short_string(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for ForwardSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.host == self.guest, self.proto) {
            (true, Proto::Tcp) => write!(f, "{}", self.host),
            (false, Proto::Tcp) => write!(f, "{}:{}", self.host, self.guest),
            (true, proto) => write!(f, "{}/{proto}", self.host),
            (false, proto) => write!(f, "{}:{}/{proto}", self.host, self.guest),
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

        // Split off optional protocol suffix.
        let (ports_part, proto) = match s.split_once('/') {
            Some((ports, proto_str)) => (ports, proto_str.parse::<Proto>()?),
            None => (s, Proto::Tcp),
        };

        // Parse host[:guest].
        let (host_str, guest_str) = match ports_part.split_once(':') {
            Some((h, g)) => (h, g),
            None => (ports_part, ports_part),
        };

        let host: u16 = parse_port(host_str).with_context(|| format!("host port in '{raw}'"))?;
        let guest: u16 = parse_port(guest_str).with_context(|| format!("guest port in '{raw}'"))?;

        Ok(Self { host, guest, proto })
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

/// Validate that a list of forward specs has no duplicate `(host, proto)` pairs.
///
/// Two forwards that bind the same host port with the same protocol would
/// conflict at runtime; catching it up front gives a clearer error than
/// letting QEMU reject the second one.
pub fn validate_unique(specs: &[ForwardSpec]) -> anyhow::Result<()> {
    for (i, a) in specs.iter().enumerate() {
        for b in &specs[i + 1..] {
            if a.host == b.host && a.proto == b.proto {
                bail!(
                    "duplicate forward for host port {}/{} in list",
                    a.host,
                    a.proto
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
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config => write!(f, "config"),
            Self::Adhoc => write!(f, "adhoc"),
        }
    }
}

/// A forward currently active on a running VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveForward {
    pub host: u16,
    pub guest: u16,
    #[serde(default)]
    pub proto: Proto,
    pub origin: Origin,
}

impl ActiveForward {
    #[must_use]
    pub fn new(spec: ForwardSpec, origin: Origin) -> Self {
        Self {
            host: spec.host,
            guest: spec.guest,
            proto: spec.proto,
            origin,
        }
    }

    #[must_use]
    pub fn spec(&self) -> ForwardSpec {
        ForwardSpec {
            host: self.host,
            guest: self.guest,
            proto: self.proto,
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parses_single_port() {
        let s: ForwardSpec = "8080".parse().unwrap();
        assert_eq!(s, ForwardSpec::new(8080, 8080, Proto::Tcp));
    }

    #[test]
    fn parses_host_guest() {
        let s: ForwardSpec = "8080:3000".parse().unwrap();
        assert_eq!(s, ForwardSpec::new(8080, 3000, Proto::Tcp));
    }

    #[test]
    fn parses_udp() {
        let s: ForwardSpec = "53/udp".parse().unwrap();
        assert_eq!(s, ForwardSpec::new(53, 53, Proto::Udp));
    }

    #[test]
    fn parses_host_guest_udp() {
        let s: ForwardSpec = "53:5353/udp".parse().unwrap();
        assert_eq!(s, ForwardSpec::new(53, 5353, Proto::Udp));
    }

    #[test]
    fn parses_explicit_tcp() {
        let s: ForwardSpec = "80/tcp".parse().unwrap();
        assert_eq!(s, ForwardSpec::new(80, 80, Proto::Tcp));
    }

    #[test]
    fn trims_whitespace() {
        let s: ForwardSpec = "  8080:3000  ".parse().unwrap();
        assert_eq!(s, ForwardSpec::new(8080, 3000, Proto::Tcp));
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
    fn rejects_unknown_proto() {
        assert!("80/sctp".parse::<ForwardSpec>().is_err());
    }

    #[test]
    fn rejects_missing_guest_with_colon() {
        assert!("80:".parse::<ForwardSpec>().is_err());
        assert!(":80".parse::<ForwardSpec>().is_err());
    }

    #[test]
    fn display_roundtrip_single_port() {
        let s = ForwardSpec::new(8080, 8080, Proto::Tcp);
        assert_eq!(s.to_string(), "8080");
        assert_eq!(s.to_string().parse::<ForwardSpec>().unwrap(), s);
    }

    #[test]
    fn display_roundtrip_host_guest() {
        let s = ForwardSpec::new(8080, 3000, Proto::Tcp);
        assert_eq!(s.to_string(), "8080:3000");
        assert_eq!(s.to_string().parse::<ForwardSpec>().unwrap(), s);
    }

    #[test]
    fn display_roundtrip_udp_same() {
        let s = ForwardSpec::new(53, 53, Proto::Udp);
        assert_eq!(s.to_string(), "53/udp");
        assert_eq!(s.to_string().parse::<ForwardSpec>().unwrap(), s);
    }

    #[test]
    fn display_roundtrip_udp_distinct() {
        let s = ForwardSpec::new(53, 5353, Proto::Udp);
        assert_eq!(s.to_string(), "53:5353/udp");
        assert_eq!(s.to_string().parse::<ForwardSpec>().unwrap(), s);
    }

    #[test]
    fn parse_specs_collects_all() {
        let raw = ["8080", "3000:5000", "53/udp"];
        let specs = parse_specs(raw).unwrap();
        assert_eq!(specs.len(), 3);
        assert_eq!(specs[0], ForwardSpec::new(8080, 8080, Proto::Tcp));
        assert_eq!(specs[1], ForwardSpec::new(3000, 5000, Proto::Tcp));
        assert_eq!(specs[2], ForwardSpec::new(53, 53, Proto::Udp));
    }

    #[test]
    fn parse_specs_reports_first_error() {
        let raw = ["8080", "not-a-port"];
        let err = parse_specs(raw).unwrap_err();
        assert!(err.to_string().contains("not-a-port"));
    }

    #[test]
    fn validate_unique_accepts_distinct() {
        let specs = vec![
            ForwardSpec::new(8080, 8080, Proto::Tcp),
            ForwardSpec::new(8081, 8080, Proto::Tcp),
            ForwardSpec::new(8080, 8080, Proto::Udp), // same port, different proto — OK
        ];
        validate_unique(&specs).unwrap();
    }

    #[test]
    fn validate_unique_rejects_duplicate() {
        let specs = vec![
            ForwardSpec::new(8080, 8080, Proto::Tcp),
            ForwardSpec::new(8080, 3000, Proto::Tcp),
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
            ActiveForward::new(ForwardSpec::new(8080, 8080, Proto::Tcp), Origin::Config),
            ActiveForward::new(ForwardSpec::new(53, 53, Proto::Udp), Origin::Adhoc),
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
                ForwardSpec::new(8080, 8080, Proto::Tcp),
                Origin::Config,
            )],
        )
        .await
        .unwrap();
        assert!(path.exists());
        // Writing empty clears the file.
        write_active(&path, &[]).await.unwrap();
        assert!(!path.exists());
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
                ForwardSpec::new(8080, 8080, Proto::Tcp),
                Origin::Config,
            )],
        )
        .await
        .unwrap();
        clear_active(&path).await.unwrap();
        assert!(!path.exists());
    }
}
