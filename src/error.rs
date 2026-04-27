//! Error types for the agv application.
//!
//! Uses `thiserror` for structured, matchable errors. Most call sites will
//! use `anyhow::Result` for convenience, but these types give library-level
//! code precise error variants to work with.

use std::path::PathBuf;

/// Top-level error enum covering all failure categories.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    // -- Config errors --
    #[error("failed to read config file {path}")]
    ConfigRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse config file {path}")]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("invalid config: {message}")]
    ConfigValidation { message: String },

    // -- VM errors --
    #[error("VM '{name}' not found")]
    VmNotFound { name: String },

    #[error("VM '{name}' already exists")]
    VmAlreadyExists { name: String },

    #[error("VM '{name}' is in state '{status}' — expected one of: {expected}")]
    VmBadState {
        name: String,
        status: String,
        expected: String,
    },

    // -- QEMU / QMP errors --
    #[error("QEMU failed to start: {message}")]
    QemuStart { message: String },

    #[error("QMP communication error: {message}")]
    Qmp { message: String },

    // -- SSH errors --
    #[error("SSH connection to VM '{name}' failed")]
    Ssh {
        name: String,
        #[source]
        source: std::io::Error,
    },

    #[error("SSH timed out waiting for VM '{name}' to become reachable")]
    SshTimeout { name: String },

    #[error("SCP transfer failed for VM '{name}'")]
    Scp {
        name: String,
        #[source]
        source: std::io::Error,
    },

    // -- Image definition errors --
    #[error("image '{name}' not found (searched built-in images and {dir})")]
    ImageNotFound { name: String, dir: PathBuf },

    #[error("circular image inheritance: {chain}")]
    CircularInheritance { chain: String },

    #[error("circular include dependency: {chain}")]
    CircularInclude { chain: String },

    #[error("include '{name}' not found (searched built-in images and user images dir)")]
    InvalidInclude { name: String },

    // -- Image errors --
    #[error("failed to download image from {url}")]
    ImageDownload {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("image checksum mismatch for {path}: expected {expected}, got {actual}")]
    ImageChecksum {
        path: PathBuf,
        expected: String,
        actual: String,
    },

    // -- Cloud-init errors --
    #[error("failed to generate cloud-init seed image")]
    CloudInit {
        #[source]
        source: std::io::Error,
    },

    // -- Template errors --
    #[error("template '{name}' not found")]
    TemplateNotFound { name: String },

    #[error("template '{name}' already exists")]
    TemplateAlreadyExists { name: String },

    #[error("template '{name}' is in use by VMs: {dependents}")]
    TemplateHasDependents { name: String, dependents: String },

    // -- Host capacity --
    /// Pre-flight capacity check refused to boot a VM because doing so
    /// would push allocated host RAM beyond the 90% threshold.
    /// Surfaced as exit code 20 (`EXIT_HOST_CAPACITY`).
    #[error("{message}")]
    HostCapacity { message: String },
}

// ---------------------------------------------------------------------------
// Exit codes
// ---------------------------------------------------------------------------

// Documented stable contract over the 0.x series. See `docs/json-schema.md`.
// 0/1/2 follow Unix conventions (and clap's exit 2 for usage errors); the
// agent-relevant codes are 10/11/12/20.

/// Generic, unexpected failure — the catch-all when no more specific
/// variant is in the error chain.
pub const EXIT_GENERIC: u8 = 1;
/// VM (or template) already exists.
pub const EXIT_ALREADY_EXISTS: u8 = 10;
/// VM, template, image, or include not found.
pub const EXIT_NOT_FOUND: u8 = 11;
/// VM is in a state that doesn't allow the requested operation, or a
/// template still has VMs depending on it.
pub const EXIT_WRONG_STATE: u8 = 12;
/// Pre-flight capacity check refused to boot — host RAM would be
/// over-committed and `--force` was not passed.
pub const EXIT_HOST_CAPACITY: u8 = 20;

/// Map an [`anyhow::Error`] chain to a documented exit code.
///
/// Walks the chain looking for the first `Error` variant we recognise
/// and returns its code. Falls back to [`EXIT_GENERIC`] when no variant
/// matches (preserves the current behaviour for any unstructured
/// failure path that hasn't been annotated yet).
#[must_use]
pub fn exit_code_for(err: &anyhow::Error) -> u8 {
    for cause in err.chain() {
        if let Some(e) = cause.downcast_ref::<Error>() {
            return match e {
                Error::VmAlreadyExists { .. } | Error::TemplateAlreadyExists { .. } => {
                    EXIT_ALREADY_EXISTS
                }
                Error::VmNotFound { .. }
                | Error::TemplateNotFound { .. }
                | Error::ImageNotFound { .. }
                | Error::InvalidInclude { .. } => EXIT_NOT_FOUND,
                Error::VmBadState { .. } | Error::TemplateHasDependents { .. } => {
                    EXIT_WRONG_STATE
                }
                Error::HostCapacity { .. } => EXIT_HOST_CAPACITY,
                // Variants that exist but don't map to a documented
                // agent-facing code yet — fall through to generic.
                Error::ConfigRead { .. }
                | Error::ConfigParse { .. }
                | Error::ConfigValidation { .. }
                | Error::QemuStart { .. }
                | Error::Qmp { .. }
                | Error::Ssh { .. }
                | Error::SshTimeout { .. }
                | Error::Scp { .. }
                | Error::CircularInheritance { .. }
                | Error::CircularInclude { .. }
                | Error::ImageDownload { .. }
                | Error::ImageChecksum { .. }
                | Error::CloudInit { .. } => EXIT_GENERIC,
            };
        }
    }
    EXIT_GENERIC
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(err: Error, expected: u8) {
        let msg = err.to_string();
        let anyhow_err: anyhow::Error = err.into();
        assert_eq!(
            exit_code_for(&anyhow_err),
            expected,
            "wrong exit code for: {msg}"
        );
    }

    #[test]
    fn exit_code_already_exists_variants() {
        check(Error::VmAlreadyExists { name: "x".into() }, EXIT_ALREADY_EXISTS);
        check(
            Error::TemplateAlreadyExists { name: "x".into() },
            EXIT_ALREADY_EXISTS,
        );
    }

    #[test]
    fn exit_code_not_found_variants() {
        check(Error::VmNotFound { name: "x".into() }, EXIT_NOT_FOUND);
        check(Error::TemplateNotFound { name: "x".into() }, EXIT_NOT_FOUND);
        check(
            Error::ImageNotFound {
                name: "x".into(),
                dir: PathBuf::from("/tmp"),
            },
            EXIT_NOT_FOUND,
        );
        check(Error::InvalidInclude { name: "x".into() }, EXIT_NOT_FOUND);
    }

    #[test]
    fn exit_code_wrong_state_variants() {
        check(
            Error::VmBadState {
                name: "x".into(),
                status: "broken".into(),
                expected: "running".into(),
            },
            EXIT_WRONG_STATE,
        );
        check(
            Error::TemplateHasDependents {
                name: "x".into(),
                dependents: "vm1".into(),
            },
            EXIT_WRONG_STATE,
        );
    }

    #[test]
    fn exit_code_host_capacity() {
        check(
            Error::HostCapacity {
                message: "overcommit".into(),
            },
            EXIT_HOST_CAPACITY,
        );
    }

    #[test]
    fn exit_code_walks_the_anyhow_chain() {
        // Errors get wrapped in `.context(...)` calls all over the codebase.
        // Exit-code mapping must look past the wrappers and find the leaf
        // Error variant — otherwise wrapped errors silently fall through to
        // EXIT_GENERIC even though the underlying type is recognised.
        let inner: anyhow::Error = Error::VmNotFound {
            name: "myvm".into(),
        }
        .into();
        let wrapped = inner.context("while looking up the VM directory");
        assert_eq!(exit_code_for(&wrapped), EXIT_NOT_FOUND);
    }

    #[test]
    fn unknown_error_falls_through_to_generic() {
        let err = anyhow::anyhow!("something exploded that isn't an Error variant");
        assert_eq!(exit_code_for(&err), EXIT_GENERIC);
    }
}
