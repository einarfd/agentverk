//! CLI definition using clap derive macros.
//!
//! All subcommands and their arguments are defined here. The rest of the
//! application matches on [`Command`] to dispatch work.

use clap::{Parser, Subcommand};

/// Create and manage QEMU VMs for AI coding agents.
#[derive(Debug, Parser)]
#[command(name = "agv", version, about, long_about = None)]
#[allow(clippy::struct_excessive_bools)]
pub struct Cli {
    /// Enable verbose output.
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Minimal output.
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Output in JSON format.
    #[arg(long, global = true)]
    pub json: bool,

    /// Assume yes for all confirmations.
    #[arg(short, long, global = true)]
    pub yes: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a new VM.
    Create(CreateArgs),

    /// Start a stopped VM.
    Start(StartArgs),

    /// Stop a running VM.
    Stop(StopArgs),

    /// Destroy a VM and delete all its data.
    Destroy(DestroyArgs),

    /// Open an SSH session to a running VM.
    Ssh(SshArgs),

    /// List all VMs.
    Ls,

    /// Show detailed information about a VM.
    Inspect(InspectArgs),

    /// Take a snapshot of a VM.
    Snapshot(SnapshotArgs),

    /// Restore a VM from a snapshot.
    Restore(RestoreArgs),

    /// Re-run provisioning on a running VM.
    Provision(ProvisionArgs),
}

#[derive(Debug, clap::Args)]
pub struct CreateArgs {
    /// Path to .toml config file (defaults to agv.toml if it exists).
    #[arg(long, value_name = "PATH")]
    pub config: Option<String>,

    /// VM name (required if not specified in config).
    #[arg(long)]
    pub name: Option<String>,

    /// Memory allocation, e.g. 2G, 512M [default: 2G].
    #[arg(long)]
    pub memory: Option<String>,

    /// Number of virtual CPUs [default: 2].
    #[arg(long)]
    pub cpus: Option<u32>,

    /// Disk size, e.g. 20G [default: 20G].
    #[arg(long)]
    pub disk: Option<String>,

    /// Base image URL (defaults to Ubuntu 24.04 for current arch).
    #[arg(long, value_name = "URL")]
    pub image: Option<String>,

    /// SHA256 checksum for image verification (format: sha256:abc123...).
    #[arg(long, value_name = "CHECKSUM")]
    pub image_checksum: Option<String>,

    /// Copy a file or directory into the VM. Repeatable. Format: source:dest.
    #[arg(long = "file", value_name = "SRC:DEST")]
    pub files: Vec<String>,

    /// Run an inline shell script during provisioning. Repeatable.
    #[arg(long = "provision", value_name = "SCRIPT")]
    pub provisions: Vec<String>,

    /// Run a script file during provisioning. Repeatable.
    #[arg(long = "provision-script", value_name = "PATH")]
    pub provision_scripts: Vec<String>,

    /// Start the VM after creation.
    #[arg(long)]
    pub start: bool,
}

#[derive(Debug, clap::Args)]
pub struct StartArgs {
    /// Name of the VM to start.
    pub name: String,
}

#[derive(Debug, clap::Args)]
pub struct StopArgs {
    /// Name of the VM to stop.
    pub name: String,

    /// Force stop (equivalent to pulling the power).
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, clap::Args)]
pub struct DestroyArgs {
    /// Name of the VM to destroy.
    pub name: String,
}

#[derive(Debug, clap::Args)]
pub struct SshArgs {
    /// Name of the VM to connect to.
    pub name: String,

    /// Command to run over SSH instead of an interactive session.
    #[arg(last = true)]
    pub command: Vec<String>,
}

#[derive(Debug, clap::Args)]
pub struct InspectArgs {
    /// Name of the VM to inspect.
    pub name: String,
}

#[derive(Debug, clap::Args)]
pub struct SnapshotArgs {
    /// Name of the VM to snapshot.
    pub name: String,

    /// Label for the snapshot.
    #[arg(long)]
    pub label: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct RestoreArgs {
    /// Name of the VM to restore.
    pub name: String,

    /// Label of the snapshot to restore.
    #[arg(long)]
    pub label: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct ProvisionArgs {
    /// Name of the VM to provision.
    pub name: String,

    /// Path to .toml config file.
    #[arg(long, value_name = "PATH")]
    pub config: Option<String>,

    /// Copy a file or directory into the VM. Repeatable. Format: source:dest.
    #[arg(long = "file", value_name = "SRC:DEST")]
    pub files: Vec<String>,

    /// Run an inline shell script. Repeatable.
    #[arg(long = "provision", value_name = "SCRIPT")]
    pub provisions: Vec<String>,

    /// Run a script file. Repeatable.
    #[arg(long = "provision-script", value_name = "PATH")]
    pub provision_scripts: Vec<String>,
}
