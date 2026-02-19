# agv

Create and manage QEMU VMs for AI coding agents.

`agv` gives each AI coding agent its own isolated Linux VM — a full development environment with SSH access, provisioned from a simple TOML config file.

## Status

Early development. The CLI scaffold is in place but commands are not yet functional.

## Requirements

- macOS (Apple Silicon) or Linux
- QEMU
  - macOS: `brew install qemu`
  - Ubuntu/Debian: `sudo apt install qemu-system`
  - Fedora: `sudo dnf install qemu-system-x86` (or `qemu-system-aarch64`)
- mkisofs or genisoimage (for cloud-init seed image generation)
  - macOS: `brew install cdrtools`
  - Ubuntu/Debian: `sudo apt install genisoimage`
  - Fedora: `sudo dnf install genisoimage`
- OpenSSH (for SSH access to VMs)
  - macOS: included with the OS
  - Linux: `sudo apt install openssh-client` (usually pre-installed)
- Rust 1.85+ (build only)

## Usage

```
agv [OPTIONS] <COMMAND>

Commands:
  create     Create a new VM
  start      Start a stopped VM
  stop       Stop a running VM
  destroy    Destroy a VM and delete all its data
  ssh        Open an SSH session to a running VM
  ls         List all VMs
  inspect    Show detailed information about a VM
  snapshot   Take a snapshot of a VM
  restore    Restore a VM from a snapshot
  provision  Re-run provisioning on a running VM

Options:
  -v, --verbose  Enable verbose output
  -q, --quiet    Minimal output
      --json     Output in JSON format
  -y, --yes      Assume yes for all confirmations
```

## Config file

VMs are configured with a TOML file (defaults to `agv.toml` in the current directory):

```toml
[vm]
name = "myenv"
memory = "4G"
cpus = 2
disk = "20G"

[[files]]
source = "~/.ssh/config"
dest = "~/.ssh/config"

[[provision]]
run = """
sudo apt-get update && sudo apt-get install -y build-essential
git clone git@github.com:org/repo.git ~/repo
"""

[[provision]]
script = "./setup.sh"
```

## Building

```
cargo build
cargo clippy
cargo test
```

## License

MIT
