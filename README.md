# agv

Create and manage QEMU VMs for AI agents.

`agv` gives each AI agent its own isolated Linux VM with SSH access, provisioned from a simple TOML config file. Supports macOS (Apple Silicon) and Linux (x86_64, aarch64).

## Installation

**Install script** (recommended — detects OS/arch, installs the binary, runs `agv doctor`):

```sh
curl -fsSL https://raw.githubusercontent.com/einarfd/agentverk/main/install.sh | sh
```

To install to a custom location:

```sh
curl -fsSL https://raw.githubusercontent.com/einarfd/agentverk/main/install.sh | sh -s -- --dest ~/.local/bin
```

**From source** (requires Rust 1.85+):

```sh
git clone https://github.com/einarfd/agentverk.git
cd agentverk
cargo install --path .
```

## Requirements

**Runtime dependencies:**

- QEMU
  - macOS: `brew install qemu`
  - Ubuntu/Debian: `sudo apt install qemu-system`
  - Fedora: `sudo dnf install qemu-system-x86` (or `qemu-system-aarch64`)
- mkisofs or genisoimage (Linux only — macOS uses the built-in `hdiutil`)
  - Ubuntu/Debian: `sudo apt install genisoimage`
  - Fedora: `sudo dnf install genisoimage`
- OpenSSH (for SSH access to VMs)
  - macOS: included with the OS
  - Linux: usually pre-installed; `sudo apt install openssh-client` if missing

Run `agv doctor` at any time to check which dependencies are present and get install instructions.

## Getting started

**With a config file** — generate one with `agv init`, then pass it to `agv create`:

```sh
agv init claude -o agv.toml                        # write a Claude Code config
agv create --config agv.toml --start myvm          # create and start the VM
agv ssh myvm                                       # open a shell inside the VM
```

See [`examples/`](examples/) for ready-to-use configs for Claude, Gemini, Codex, and OpenClaw.

**Without a config file** — pass everything on the command line:

```sh
agv create --include devtools --include claude --start myvm  # uses the default spec (medium: 2G RAM, 2 vCPUs, 20G disk)
```

Use `agv images` to see all available mixins, and `agv specs` to see size presets.
`agv create` does **not** pick up `agv.toml` from the current directory — you must pass `--config` explicitly.

**IDE integration** — set up once, then every running VM is accessible by name from
VS Code, JetBrains, plain `ssh`, and any other SSH-based tool:

```sh
agv doctor --setup-ssh   # add Include to ~/.ssh/config (one-time)
ssh myvm                 # connect directly by VM name
```

See [`docs/remote-ide.md`](docs/remote-ide.md) for IDE-specific setup.

## Usage

```
agv [OPTIONS] <COMMAND>

Commands:
  create    Create a new VM (use --interactive to step through provisioning)
  start     Start a stopped VM (--retry to resume failed provisioning, --interactive to step)
  stop      Stop a running VM
  suspend   Suspend a running VM (save full state to disk)
  resume    Resume a suspended VM
  destroy   Destroy a VM and delete all its data
  ssh       Open an SSH session to a running VM
  cp        Copy files between the host and a running VM
  forward   Forward ports from a running VM to the host
  ls        List all VMs
  images    List available base images and mixins
  inspect   Show runtime status of a VM
  config    View or change VM configuration
  template  Create and manage VM templates
  specs     List available hardware size presets
  cache     Manage the image download cache
  init      Write a starter agv.toml to the current directory
  doctor    Check dependencies and set up SSH config integration

Options:
  -v, --verbose  Enable verbose output
  -q, --quiet    Minimal output
      --json     Output in JSON format
  -y, --yes      Assume yes for all confirmations
```

## Config file

VMs can be configured with a TOML file passed to `agv create --config <path>`.
Run `agv init -o <path>` to generate a starter file, or `agv specs` to see available size presets.
See [`docs/config.md`](docs/config.md) for the full reference including CLI equivalents for every field.

```toml
[base]
from = "ubuntu-24.04"
include = ["devtools", "claude"]
spec = "large"  # 8G RAM, 4 vCPUs, 40G disk

# Override individual resource settings if needed:
# [vm]
# memory = "16G"
# disk = "80G"

# Copy files into the VM (use {{HOME}} not ~/, see docs/config.md):
[[files]]
source = "{{HOME}}/.gitconfig"
dest   = "/home/{{AGV_USER}}/.gitconfig"

# Run as root during OS setup:
[[setup]]
run = "apt-get install -y ripgrep"

# Run as your user after setup:
[[provision]]
run = "git clone git@github.com:org/repo.git ~/repo"

[[provision]]
script = "./bootstrap.sh"
```

## Templates

Convert a provisioned VM into a reusable base image, then stamp out thin clones:

```sh
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
  - macOS: `xcode-select --install`
  - Ubuntu/Debian: `sudo apt install build-essential`
  - Fedora: `sudo dnf install gcc`

**Build and test:**

```sh
cargo build           # debug binary → ./target/debug/agv
cargo build --release # release binary → ./target/release/agv
cargo clippy          # lint — must pass with zero warnings
cargo test            # unit and integration tests (fast, no QEMU required)
```

## Documentation

- [`docs/config.md`](docs/config.md) — full config file reference with CLI equivalents
- [`docs/repo-access.md`](docs/repo-access.md) — accessing private repositories (PAT, SSH keys, deploy keys)
- [`docs/remote-ide.md`](docs/remote-ide.md) — connecting VS Code, JetBrains, and other IDEs to VMs

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT License ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.
