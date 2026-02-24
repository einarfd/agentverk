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
    Create(Box<CreateArgs>),

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

    /// List available images.
    Images,

    /// Show detailed information about a VM.
    Inspect(InspectArgs),

    /// Create and manage VM templates.
    Template(Box<TemplateArgs>),

    /// Manage the image download cache.
    Cache(CacheArgs),

    /// List available VM hardware specs.
    Specs,
}

#[derive(Debug, clap::Args)]
pub struct CreateArgs {
    /// Name for the new VM instance.
    pub name: String,

    /// Path to .toml config file (defaults to agv.toml if it exists).
    #[arg(long, value_name = "PATH")]
    pub config: Option<String>,

    /// Image to base the VM on [default: ubuntu-24.04].
    #[arg(long)]
    pub image: Option<String>,

    /// Hardware spec to use [default: medium].
    #[arg(long)]
    pub spec: Option<String>,

    /// Memory allocation, e.g. 2G, 512M.
    #[arg(long)]
    pub memory: Option<String>,

    /// Number of virtual CPUs.
    #[arg(long)]
    pub cpus: Option<u32>,

    /// Disk size, e.g. 20G.
    #[arg(long)]
    pub disk: Option<String>,

    /// Include a named module (files/setup/provision). Repeatable.
    #[arg(long = "include", value_name = "NAME")]
    pub includes: Vec<String>,

    /// Copy a file or directory into the VM. Repeatable. Format: source:dest.
    #[arg(long = "file", value_name = "SRC:DEST")]
    pub files: Vec<String>,

    /// Run an inline shell script as root during setup. Repeatable.
    #[arg(long = "setup", value_name = "SCRIPT")]
    pub setups: Vec<String>,

    /// Run a script file as root during setup. Repeatable.
    #[arg(long = "setup-script", value_name = "PATH")]
    pub setup_scripts: Vec<String>,

    /// Run an inline shell script during provisioning. Repeatable.
    #[arg(long = "provision", value_name = "SCRIPT")]
    pub provisions: Vec<String>,

    /// Run a script file during provisioning. Repeatable.
    #[arg(long = "provision-script", value_name = "PATH")]
    pub provision_scripts: Vec<String>,

    /// Skip image checksum verification.
    #[arg(long)]
    pub no_checksum: bool,

    /// Start the VM after creation.
    #[arg(long)]
    pub start: bool,

    /// Create VM as a thin clone of this template instead of building from scratch.
    #[arg(long, value_name = "TEMPLATE", conflicts_with_all = ["config", "image"])]
    pub from: Option<String>,
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
pub struct CacheArgs {
    #[command(subcommand)]
    pub command: CacheCommand,
}

#[derive(Debug, Subcommand)]
pub enum CacheCommand {
    /// List cached images and their disk usage.
    Ls,

    /// Remove cached images that are no longer referenced by any VM.
    Clean,
}

#[derive(Debug, clap::Args)]
pub struct TemplateArgs {
    #[command(subcommand)]
    pub command: TemplateCommand,
}

#[derive(Debug, Subcommand)]
pub enum TemplateCommand {
    /// Create a template from an existing VM.
    Create(TemplateCreateArgs),

    /// List available templates.
    Ls,

    /// Delete a template.
    Rm(TemplateRmArgs),
}

#[derive(Debug, clap::Args)]
pub struct TemplateRmArgs {
    /// Name of the template to delete.
    pub name: String,
}

#[derive(Debug, clap::Args)]
pub struct TemplateCreateArgs {
    /// Name of the VM to convert into a template.
    pub vm: String,

    /// Name for the new template.
    pub name: String,

    /// Stop the VM first if it is currently running.
    #[arg(long)]
    pub stop: bool,
}

