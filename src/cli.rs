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

    /// Suspend a running VM, saving its full state to disk.
    Suspend(SuspendArgs),

    /// Resume a suspended VM from its saved state.
    Resume(ResumeArgs),

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

    /// View or change VM configuration.
    Config(Box<ConfigArgs>),

    /// Forward ports from a running VM to the host.
    ///
    /// Each port is local[:remote]. If remote is omitted, it matches local.
    ///
    /// Examples:
    ///   agv forward myvm 8080               # VM:8080 → local:8080
    ///   agv forward myvm 8080:3000          # VM:3000 → local:8080
    ///   agv forward myvm 8080:3000 5432     # forward two ports
    #[command(verbatim_doc_comment)]
    Forward(ForwardArgs),

    /// Copy files between the host and a running VM.
    ///
    /// Prefix paths with : to indicate a path inside the VM.
    ///
    /// Examples:
    ///   agv cp myvm :~/file.txt ./              # download from VM
    ///   agv cp myvm ./file.txt :~/              # upload to VM
    ///   agv cp myvm -r :~/project/ ./local/     # recursive download
    ///   agv cp myvm -r ./local/dir/ :~/remote/  # recursive upload
    #[command(verbatim_doc_comment)]
    Cp(CpArgs),

    /// Check that all required external tools are installed.
    Doctor(DoctorArgs),

    /// Write a starter agv.toml to the current directory.
    Init(InitArgs),
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
pub struct SuspendArgs {
    /// Name of the VM to suspend.
    pub name: String,
}

#[derive(Debug, clap::Args)]
pub struct ResumeArgs {
    /// Name of the VM to resume.
    pub name: String,
}

#[derive(Debug, clap::Args)]
pub struct DestroyArgs {
    /// Name of the VM to destroy.
    pub name: String,

    /// Destroy even if the VM is currently running (force-stops it first).
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, clap::Args)]
pub struct SshArgs {
    /// Name of the VM to connect to.
    pub name: String,

    /// SSH options and/or remote command.
    /// Options (e.g. -A, -L 8080:localhost:8080) are passed through to ssh.
    /// Use -- to separate ssh options from a remote command.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
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
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Show the full resolved configuration of a VM.
    Show(ConfigShowArgs),

    /// Change hardware settings of a stopped VM (memory, CPUs, disk).
    ///
    /// The VM must be stopped or broken. Disk can only be grown, not shrunk.
    /// The guest filesystem is not resized automatically — run growpart/resize2fs
    /// inside the VM after the next start to use the extra disk space.
    Set(ConfigSetArgs),
}

#[derive(Debug, clap::Args)]
pub struct ConfigShowArgs {
    /// Name of the VM to show configuration for.
    pub name: String,
}

#[derive(Debug, clap::Args)]
pub struct ConfigSetArgs {
    /// Name of the VM to reconfigure.
    pub name: String,

    /// New memory allocation, e.g. 4G, 8G.
    #[arg(long)]
    pub memory: Option<String>,

    /// New number of virtual CPUs.
    #[arg(long)]
    pub cpus: Option<u32>,

    /// New disk size (must be larger than current), e.g. 40G.
    #[arg(long)]
    pub disk: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct DoctorArgs {
    /// Add an Include line to ~/.ssh/config so you can ssh into VMs by name and IDEs can connect automatically.
    #[arg(long)]
    pub setup_ssh: bool,

    /// Remove the agv Include line from ~/.ssh/config.
    #[arg(long)]
    pub remove_ssh: bool,
}

#[derive(Debug, clap::Args)]
pub struct ForwardArgs {
    /// Name of the VM to forward ports from.
    pub name: String,

    /// Ports to forward: local[:remote]. Repeatable.
    #[arg(required = true)]
    pub ports: Vec<String>,
}

#[derive(Debug, clap::Args)]
pub struct CpArgs {
    /// Name of the VM to copy files to/from.
    pub name: String,

    /// Source path. Prefix with : for a path inside the VM.
    pub source: String,

    /// Destination path. Prefix with : for a path inside the VM.
    pub dest: String,

    /// Copy directories recursively.
    #[arg(short, long)]
    pub recursive: bool,
}

#[derive(Debug, clap::Args)]
pub struct InitArgs {
    /// Template to use: claude, gemini, codex, openclaw.
    /// Writes a minimal annotated config if not specified.
    pub template: Option<String>,

    /// Overwrite an existing agv.toml.
    #[arg(long)]
    pub force: bool,
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

