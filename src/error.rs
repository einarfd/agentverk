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
}
