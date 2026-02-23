# agv

Create and manage QEMU VMs for AI coding agents.

`agv` gives each AI coding agent its own isolated Linux VM — a full development environment with SSH access, provisioned from a simple TOML config file.

## Requirements

Supported platforms: macOS (Apple Silicon) or Linux (x86_64 or aarch64).

**Runtime dependencies** (needed to run `agv`):

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
  - Linux: usually pre-installed; `sudo apt install openssh-client` if missing

## Usage

```
agv [OPTIONS] <COMMAND>

Commands:
  create    Create a new VM
  start     Start a stopped VM
  stop      Stop a running VM
  destroy   Destroy a VM and delete all its data
  ssh       Open an SSH session to a running VM
  ls        List all VMs
  images    List available images
  inspect   Show detailed information about a VM
  template  Create and manage VM templates
  cache     Manage the image download cache

Options:
  -v, --verbose  Enable verbose output
  -q, --quiet    Minimal output
      --json     Output in JSON format
  -y, --yes      Assume yes for all confirmations
```

## Config file

VMs are configured with a TOML file (defaults to `agv.toml` in the current directory):

```toml
[base]
from = "ubuntu-24.04"

include = ["devtools"]

[vm]
memory = "4G"
cpus = 2
disk = "20G"

[[files]]
source = "~/.ssh/config"
dest = "~/.ssh/config"

[[setup]]
run = "sudo apt-get update && sudo apt-get install -y build-essential"

[[provision]]
run = "git clone git@github.com:org/repo.git ~/repo"

[[provision]]
script = "./setup.sh"
```

## Templates

Convert a provisioned VM into a reusable base image, then stamp out thin clones:

```bash
agv template create myvm mytemplate   # create template from VM
agv template ls                        # list templates
agv create --from mytemplate newvm     # create thin clone
```

## Building from source

**Build dependencies:**

- Rust 1.85 or later — install via [rustup](https://rustup.rs):
  ```
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  ```
- A C linker (usually already present)
  - macOS: install Xcode Command Line Tools: `xcode-select --install`
  - Ubuntu/Debian: `sudo apt install build-essential`
  - Fedora: `sudo dnf install gcc`

**Build:**

```
cargo build           # debug binary → ./target/debug/agv
cargo build --release # release binary → ./target/release/agv
```

**Lint and test:**

```
cargo clippy          # must pass with zero warnings
cargo test            # unit and integration tests (fast, no QEMU required)
```

Some tests boot a real VM and are skipped by default. To run them (requires
QEMU and a network connection to download the base image):

```
cargo test -- --include-ignored --nocapture
```

## License

MIT
