//! CLI definition using clap derive macros.
//!
//! All subcommands and their arguments are defined here. The rest of the
//! application matches on [`Command`] to dispatch work.

use clap::{Parser, Subcommand};

/// Create and manage QEMU VMs for AI agents.
#[derive(Debug, Parser)]
#[command(name = "agv", version = env!("AGV_VERSION"), about, long_about = None)]
pub struct Cli {
    /// Enable verbose output.
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Minimal output.
    #[arg(short, long, global = true)]
    pub quiet: bool,

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

    /// Rename a VM. The VM must be stopped or suspended.
    Rename(RenameArgs),

    /// Open an SSH session to a running VM.
    Ssh(SshArgs),

    /// Open the VM's XFCE desktop in the host browser.
    ///
    /// Requires the VM to include a GUI mixin (e.g. `gui-xfce`). The mixin
    /// runs `TigerVNC` + `noVNC` inside the guest, bound to `127.0.0.1`
    /// and with `-SecurityTypes None`. agv tunnels the HTTP port through SSH
    /// and opens the browser at the tunneled URL — the SSH tunnel (with
    /// the VM's unique ed25519 key) is the auth boundary, so no password
    /// ever hits the URL or browser history.
    Gui(GuiArgs),

    /// List all VMs.
    Ls(LsArgs),

    /// List available images.
    Images(ImagesArgs),

    /// Show detailed information about a VM.
    Inspect(InspectArgs),

    /// Create and manage VM templates.
    Template(Box<TemplateArgs>),

    /// Manage the image download cache.
    Cache(CacheArgs),

    /// List available VM hardware specs.
    Specs(SpecsArgs),

    /// Show host capacity (RAM, CPUs, disk) and what agv has allocated.
    ///
    /// Useful before creating a VM to confirm the host has the headroom
    /// for the requested spec, especially when running multiple VMs.
    /// Pass `--json` for machine-readable output.
    Resources(ResourcesArgs),

    /// View or change VM configuration.
    Config(Box<ConfigArgs>),

    /// Add, list, or remove host-to-guest port forwards on a running VM.
    ///
    /// Port specs use the form HOST[:GUEST]. TCP is implicit — the
    /// underlying `ssh -L` tunnel is TCP-only.
    ///
    ///   agv forward myvm 8080               # host:8080 → VM:8080
    ///   agv forward myvm 8080:3000          # host:8080 → VM:3000
    ///   agv forward myvm 5432 9090          # add two at once
    ///   agv forward myvm --list             # show active forwards
    ///   agv forward myvm --stop             # remove every active forward
    ///   agv forward myvm --stop 8080        # remove a specific forward
    ///
    /// Runtime changes are ephemeral: on next start/resume the set is reset
    /// to what the config declares in `forwards = [...]`.
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

    /// Internal: supervisor loop for a single port forward.
    ///
    /// Not meant for end users — spawned by `agv forward` and by
    /// start/resume to keep an `ssh -N -L` tunnel alive. Exits when sent
    /// SIGTERM/SIGINT or when agv stops the forward.
    #[command(name = "__forward-daemon", hide = true)]
    ForwardDaemon(ForwardDaemonArgs),

    /// Internal: per-VM auto-suspend supervisor.
    ///
    /// Not meant for end users — spawned by `agv start`/`agv resume` when
    /// `idle_suspend_minutes > 0`. Polls the guest for activity and
    /// triggers `agv suspend` after the configured number of idle
    /// minutes. Exits when sent SIGTERM/SIGINT or after a successful
    /// auto-suspend.
    #[command(name = "__idle-watcher", hide = true)]
    IdleWatcher(IdleWatcherArgs),

    /// Write a starter config file to a given path (use with `agv create --config`).
    Init(InitArgs),
}

#[derive(Debug, clap::Args)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each bool maps to a distinct CLI flag (--no-checksum, --force, --start, --interactive); refactoring would only obscure the clap-derive mapping"
)]
pub struct CreateArgs {
    /// Name for the new VM instance. Agents driving agv programmatically
    /// should pick a clearly-owned pattern (e.g. `agv-<task>-<short-id>`)
    /// so multiple agents coexist without collisions; pair with
    /// `--label session=<id>` for cleanup via `agv destroy --label`.
    pub name: String,

    /// Path to .toml config file.
    #[arg(short, long, value_name = "PATH")]
    pub config: Option<String>,

    /// Path to a .env file. Layered on top of `.env` next to the agv.toml
    /// and `.env` in the current directory; host environment variables
    /// still win over all three.
    ///
    /// Note: any `{{VAR}}` values that get template-expanded into the
    /// VM's resolved config are baked into the saved instance config
    /// (`<data_dir>/instances/<name>/config.toml`) and, depending on
    /// where they're referenced, may also land in shell rc files inside
    /// the VM. `agv destroy` removes both.
    #[arg(long = "env-file", value_name = "PATH")]
    pub env_file: Option<String>,

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

    /// Add a mixin to the VM (e.g. devtools, claude, docker, rust, nodejs).
    ///
    /// Mixins are named bundles of setup/provision steps that install and
    /// configure specific tools or languages. Run `agv images` to see all
    /// available mixins. Repeat the flag to add multiple:
    ///   --include devtools --include claude
    #[arg(short = 'i', long = "include", value_name = "NAME")]
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

    /// Skip the host-capacity preflight check.
    ///
    /// By default `agv create --start` refuses to boot a VM when its
    /// memory plus the memory committed to already-running VMs would
    /// exceed 90% of host RAM. `--force` bypasses that check; useful
    /// when you know you're about to suspend or stop something else,
    /// or when sysinfo's reading is wrong on your host.
    #[arg(long)]
    pub force: bool,

    /// Succeed silently when a VM with this name already exists.
    ///
    /// Useful for AI agents that can't reliably track session state — they
    /// can `agv create --if-not-exists agv-session-X` without first
    /// running `agv ls`. With `--json`, the existing VM's current state is
    /// printed; the `created` field is `false` to signal "already there".
    /// This flag does not change `--start`'s behavior on an existing VM
    /// (use `agv start` separately if you also need it running).
    #[arg(long = "if-not-exists")]
    pub if_not_exists: bool,

    /// Output the new (or existing, with `--if-not-exists`) VM's state as
    /// JSON on success. Saves a follow-up `agv inspect` round trip when
    /// scripting against `agv create`.
    #[arg(long)]
    pub json: bool,

    /// Attach a free-form `key=value` label to the VM. Repeatable.
    /// Bare `key` (no `=`) is shorthand for `key=""`. Labels are stored
    /// with the VM and surfaced via `agv inspect`, `agv ls --labels`,
    /// and the `labels` field of `--json` output. Filterable via
    /// `agv ls --label k=v` and `agv destroy --label k=v`. agv doesn't
    /// interpret label contents — they're for you (or your agent) to
    /// track which VMs you own.
    #[arg(long = "label", value_name = "K=V")]
    pub labels: Vec<String>,

    /// Start the VM after creation.
    #[arg(short, long)]
    pub start: bool,

    /// Prompt before each provisioning step (y/n/e/a/q).
    /// Useful for debugging or stepping through a script.
    #[arg(long)]
    pub interactive: bool,

    /// Create VM as a thin clone of this template instead of building from scratch.
    #[arg(long, value_name = "TEMPLATE", conflicts_with_all = ["config", "image"])]
    pub from: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct StartArgs {
    /// Name of the VM to start.
    pub name: String,

    /// Resume provisioning of a broken VM from where it failed.
    /// Skips already-completed setup, file-copy, and provision steps.
    #[arg(long)]
    pub retry: bool,

    /// Prompt before each provisioning step (y/n/e/a/q).
    /// Useful for debugging or stepping through a script.
    #[arg(long)]
    pub interactive: bool,

    /// Output the VM's post-start state as a JSON `VmStateReport`.
    /// Implies `--quiet` for progress chrome so JSON parsing isn't
    /// broken by spinner residue.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct StopArgs {
    /// Name of the VM to stop.
    pub name: String,

    /// Force stop (equivalent to pulling the power).
    #[arg(short, long)]
    pub force: bool,

    /// Output the VM's post-stop state as a JSON `VmStateReport`.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct SuspendArgs {
    /// Name of the VM to suspend.
    pub name: String,

    /// Output the VM's post-suspend state as a JSON `VmStateReport`.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct ResumeArgs {
    /// Name of the VM to resume.
    pub name: String,

    /// Output the VM's post-resume state as a JSON `VmStateReport`.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct DestroyArgs {
    /// Name of the VM to destroy. Optional when `--label` is given —
    /// bulk-destroy by label selector instead of naming a single VM.
    pub name: Option<String>,

    /// Destroy every VM whose labels match this `key=value` selector.
    /// Repeatable; multiple `--label` filters AND together. Bare
    /// `key` (no `=`) matches any value. With `--force`, running VMs
    /// are torn down too; otherwise the bulk delete is refused if any
    /// matched VM is running. Without `-y`, agv lists matched VMs and
    /// prompts before doing anything.
    #[arg(long = "label", value_name = "K=V", conflicts_with = "name")]
    pub label: Vec<String>,

    /// Destroy even if the VM is currently running (force-stops it first).
    #[arg(short, long)]
    pub force: bool,

    /// Output a small JSON object on success: `{ "name": "...",
    /// "destroyed": true }`. Intentionally a different shape from
    /// `VmStateReport` since the VM no longer exists. With label-based
    /// bulk destroy, emits a JSON array of these.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct RenameArgs {
    /// Current name of the VM.
    pub old: String,

    /// New name for the VM.
    pub new: String,

    /// Output the renamed VM's state as a JSON `VmStateReport`.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct SshArgs {
    /// Name of the VM to connect to.
    pub name: String,

    /// SSH options and/or remote command.
    /// Options (e.g. -A, -L 8080:localhost:8080) are passed through to ssh.
    /// Use -- to separate ssh options from a remote command.
    ///
    /// Note: clap's `trailing_var_arg` silently consumes a leading `--`
    /// so this `Vec<String>` may not include the `--` the user typed.
    /// `split_ssh_args` re-detects that case from `std::env::args_os()`
    /// and routes accordingly.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

#[derive(Debug, clap::Args)]
pub struct GuiArgs {
    /// Name of the VM whose desktop to open.
    pub name: String,

    /// Print the URL but don't open the browser. Useful when you want
    /// to copy-paste the URL into a specific browser or profile.
    #[arg(long)]
    pub no_launch: bool,
}

#[derive(Debug, clap::Args)]
pub struct InspectArgs {
    /// Name of the VM to inspect.
    pub name: String,

    /// Output the VM's state as JSON instead of a human-readable summary.
    /// Same shape as `agv create --json` (a `VmStateReport`).
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct LsArgs {
    /// Output as a JSON array of `VmStateReport` objects (same shape as
    /// `agv inspect --json` and `agv create --json`).
    #[arg(long)]
    pub json: bool,

    /// Show the `labels` column in human output. Hidden by default to
    /// keep the table compact when no labels are in use. The labels
    /// field is always present in `--json` output regardless.
    #[arg(long)]
    pub labels: bool,

    /// Filter to VMs with this `key=value` label set. Repeatable;
    /// multiple `--label` filters AND together. A bare `key` (no `=`)
    /// matches any VM that has that key, regardless of value.
    #[arg(long = "label", value_name = "K=V")]
    pub label: Vec<String>,
}

#[derive(Debug, clap::Args)]
pub struct ResourcesArgs {
    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct ImagesArgs {
    /// Output as a JSON array of image entries.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct SpecsArgs {
    /// Output as a JSON array of spec entries.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct CacheArgs {
    #[command(subcommand)]
    pub command: CacheCommand,
}

#[derive(Debug, Subcommand)]
pub enum CacheCommand {
    /// List cached images and their disk usage.
    Ls(CacheLsArgs),

    /// Remove cached images that are no longer referenced by any VM.
    Clean,
}

#[derive(Debug, clap::Args)]
pub struct CacheLsArgs {
    /// Output as a JSON array of cache entries.
    #[arg(long)]
    pub json: bool,
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
    Ls(TemplateLsArgs),

    /// Delete a template.
    Rm(TemplateRmArgs),
}

#[derive(Debug, clap::Args)]
pub struct TemplateLsArgs {
    /// Output as a JSON array of template entries.
    #[arg(long)]
    pub json: bool,
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

    /// Replace the persistent forwards list with a comma-separated set of
    /// specs (HOST[:GUEST]). Pass an empty string to clear all
    /// forwards. Takes effect on the next start/resume.
    #[arg(long, value_name = "SPECS")]
    pub forwards: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct DoctorArgs {
    /// Add an Include line to ~/.ssh/config so you can ssh into VMs by name and IDEs can connect automatically.
    #[arg(long)]
    pub setup_ssh: bool,

    /// Remove the agv Include line from ~/.ssh/config.
    #[arg(long)]
    pub remove_ssh: bool,

    /// Output the dependency report as JSON.
    #[arg(long, conflicts_with_all = ["setup_ssh", "remove_ssh"])]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct ForwardDaemonArgs {
    /// Name of the VM to forward to.
    pub name: String,

    /// Forward spec in HOST[:GUEST] form.
    pub spec: String,
}

#[derive(Debug, clap::Args)]
pub struct IdleWatcherArgs {
    /// Name of the VM to watch.
    pub name: String,

    /// Suspend after this many minutes of confirmed idleness.
    pub threshold_minutes: u32,

    /// Guest 5-min load average below which the VM counts as idle.
    pub load_threshold: f32,
}

#[derive(Debug, clap::Args)]
pub struct ForwardArgs {
    /// Name of the VM.
    pub name: String,

    /// Port specs (HOST[:GUEST]). With no flags, each spec is added;
    /// with --stop, each spec is removed. Cannot be combined with --list.
    #[arg(conflicts_with = "list")]
    pub ports: Vec<String>,

    /// Show the active forwards on the VM.
    #[arg(long, conflicts_with = "stop")]
    pub list: bool,

    /// Remove forwards. With port specs, only those are removed; with no
    /// specs, every active forward (config and ad-hoc) is removed.
    #[arg(long)]
    pub stop: bool,

    /// Output as JSON. With `--list`, prints an array of active forwards;
    /// without `--list`, ignored.
    #[arg(long)]
    pub json: bool,
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

    /// Path to write the config to, e.g. ./agv.toml.
    #[arg(short, long, value_name = "PATH")]
    pub output: String,

    /// Overwrite the output file if it already exists.
    #[arg(short, long)]
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin clap's quirky behaviour for `agv ssh <name> -- <cmd>`.
    /// `trailing_var_arg + allow_hyphen_values` silently consumes a
    /// *leading* `--`, so this `args` field will NOT contain it.
    /// The runtime recovers the boundary by inspecting raw argv —
    /// see `split_ssh_args` and `raw_argv_has_leading_dash_dash_after_ssh`
    /// in `lib.rs`. This test exists to make the silent eating
    /// loud if clap ever changes behaviour, since the recovery
    /// path is only needed because of it.
    #[test]
    fn ssh_clap_swallows_leading_dash_dash() {
        let cli = Cli::try_parse_from(["agv", "ssh", "myvm", "--", "cat /tmp/foo"])
            .expect("agv ssh ... -- ... should parse");
        let Command::Ssh(ssh) = cli.command else {
            panic!("expected Command::Ssh");
        };
        assert_eq!(ssh.name, "myvm");
        assert_eq!(
            ssh.args,
            vec!["cat /tmp/foo".to_string()],
            "clap discards the `--` here — this is the quirk",
        );
    }

    /// When at least one non-`--` value precedes `--`, clap *does*
    /// preserve `--` in `args`. The runtime's `split_ssh_args` then
    /// uses it directly without consulting raw argv.
    #[test]
    fn ssh_clap_preserves_dash_dash_after_value() {
        let cli = Cli::try_parse_from([
            "agv", "ssh", "myvm", "-A", "--", "ls", "-la",
        ])
        .expect("agv ssh myvm -A -- ls -la should parse");
        let Command::Ssh(ssh) = cli.command else {
            panic!("expected Command::Ssh");
        };
        assert_eq!(
            ssh.args,
            vec![
                "-A".to_string(),
                "--".to_string(),
                "ls".to_string(),
                "-la".to_string(),
            ],
        );
    }

    /// `agv ssh myvm -A` (no command, just an ssh option) — `args`
    /// has just the option.
    #[test]
    fn ssh_clap_accepts_lone_hyphen_value() {
        let cli = Cli::try_parse_from(["agv", "ssh", "myvm", "-A"])
            .expect("agv ssh myvm -A should parse");
        let Command::Ssh(ssh) = cli.command else {
            panic!("expected Command::Ssh");
        };
        assert_eq!(ssh.args, vec!["-A".to_string()]);
    }

    /// `agv ssh myvm` (bare interactive) parses with empty args.
    #[test]
    fn ssh_clap_bare_interactive() {
        let cli = Cli::try_parse_from(["agv", "ssh", "myvm"])
            .expect("agv ssh myvm should parse");
        let Command::Ssh(ssh) = cli.command else {
            panic!("expected Command::Ssh");
        };
        assert!(ssh.args.is_empty());
    }
}

