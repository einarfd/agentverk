# agv

Create and manage microVMs for AI agents.

`agv` gives each AI agent its own isolated Linux VM with SSH access, provisioned from a simple TOML config file. Supports macOS (Apple Silicon) and Linux (x86_64, aarch64).

**Two backends, picked per VM:**

- `qemu` — QEMU process. Cross-platform. Default everywhere except macOS on Apple Silicon. Supports `agv suspend` / `agv resume` and `idle_suspend_minutes` auto-suspend.
- `avf` — Apple Virtualization (`Virtualization.framework`). macOS Apple Silicon only. Default on that host shape. Faster cold boot than QEMU. Does **not** support `agv suspend` / `agv resume` or `idle_suspend_minutes` — Apple's framework doesn't support save/restore for Linux guests. Use `agv stop` + `agv start` instead, or pick the `qemu` backend if auto-suspend matters.

Override per VM with `--backend avf` / `--backend qemu` on `agv create`, or `backend = "..."` in the TOML config. See `docs/config.md` for the full reference.

## Installation

**Install script** (recommended — detects OS/arch, installs the binary, runs `agv doctor`):

```sh
curl -fsSL https://raw.githubusercontent.com/einarfd/agentverk/main/install.sh | sh
```

To install to a custom location:

```sh
curl -fsSL https://raw.githubusercontent.com/einarfd/agentverk/main/install.sh | sh -s -- --dest ~/.local/bin
```

**From crates.io** (if you already have Rust 1.85+):

```sh
cargo install agv
```

**From source** (latest `main`, requires Rust 1.85+):

```sh
git clone https://github.com/einarfd/agentverk.git
cd agentverk
just install     # cargo install + builds + installs agv-avf-runner on macOS
```

If you don't have [just](https://just.systems), the equivalent is `cargo install --path .` plus — on macOS Apple Silicon, where the `avf` backend is the default — `just build-avf-runner` (or `swift build -c release` in `swift/avf-runner/` plus the codesign step from the recipe) and copying `swift/avf-runner/.build/release/agv-avf-runner` next to the installed `agv` binary (typically `~/.cargo/bin/`). Without the sibling runner, `agv create` falls back to QEMU and `--backend avf` fails with a clear `agv doctor` hint.

## Requirements

**Runtime dependencies:**

- QEMU (required — `qemu-img` is used to build disk overlays, and the `qemu` backend uses `qemu-system-*` to boot)
  - macOS: `brew install qemu`
  - Ubuntu/Debian: `sudo apt install qemu-system`
  - Fedora: `sudo dnf install qemu-system-x86` (or `qemu-system-aarch64`)
- mkisofs or genisoimage (Linux only — macOS uses the built-in `hdiutil`)
  - Ubuntu/Debian: `sudo apt install genisoimage`
  - Fedora: `sudo dnf install genisoimage`
- OpenSSH (for SSH access to VMs)
  - macOS: included with the OS
  - Linux: usually pre-installed; `sudo apt install openssh-client` if missing
- `agv-avf-runner` (macOS Apple Silicon only — required for the AVF backend)
  - Bundled with release tarballs (installs alongside the `agv` binary).
  - Source installs: `just install` handles this. Manual fallback: `just build-avf-runner` in the agv repo, then move the resulting `.build/release/agv-avf-runner` next to your installed `agv` binary (or set `AGV_AVF_RUNNER=/path/to/agv-avf-runner`).

Run `agv doctor` at any time to check which dependencies are present and get install instructions.

## Getting started

**With a config file** — generate one with `agv init`, then pass it to `agv create`:

```sh
agv init claude -o agv.toml                        # write a Claude Code config
agv create --config agv.toml --start myvm          # create and start the VM
agv ssh myvm                                       # open a shell inside the VM
```

See [`examples/`](examples/) for ready-to-use configs for Claude, Gemini, Codex, OpenClaw, and a browser-based XFCE desktop.

**Without a config file** — pass everything on the command line:

```sh
agv create --include devtools --include claude --start myvm  # uses the default spec (medium: 2G RAM, 2 vCPUs, 20G disk)
```

**Picking a base image** — Ubuntu 24.04 is the default if `--image` is omitted.
`--image ubuntu`, `--image debian`, and `--image fedora` are shorthands for the
current canonical versions (`ubuntu-24.04`, `debian-12`, `fedora-43`). Mixins
like `devtools`, `nodejs`, `gh`, `rust`, `zsh`, and `oh-my-zsh` work across
every supported family, so `--image fedora --include devtools` is as
straightforward as the Ubuntu case.

```sh
agv create --image fedora --include devtools --start myfedora
```

Use `agv images` to see all available base images and mixins, and `agv specs`
to see size presets. `agv create` does **not** pick up `agv.toml` from the
current directory — you must pass `--config` explicitly.

**IDE integration** — set up once, then every running VM is accessible by name from
VS Code, JetBrains, plain `ssh`, and any other SSH-based tool:

```sh
agv doctor --setup-ssh   # add Include to ~/.ssh/config (one-time)
ssh myvm                 # connect directly by VM name
```

See [`docs/remote-ide.md`](docs/remote-ide.md) for IDE-specific setup.

**Desktop / GUI access** — add the `gui-xfce` mixin and open the VM's
XFCE desktop in your default browser. No native VNC/RDP client needed on
the host; the SSH tunnel (keyed by the VM's ed25519 key) is the auth
boundary.

```sh
agv create --include devtools --include gui-xfce --start myvm
agv gui myvm
```

See [`examples/gui/agv.toml`](examples/gui/agv.toml) for a ready-to-use
config and [`docs/config.md`](docs/config.md) for the auth model.

**Port forwards** — run a Vite/Next.js/Django/etc. dev server (or any
other service) inside the VM and reach it from your host browser at
`http://localhost:PORT`. Declare persistent forwards in `agv.toml`:

```toml
[[forwards]]
host = 8080        # host:8080 → VM:8080

[[forwards]]
host = 5433
guest = 5432       # host:5433 → VM:5432
```

`guest` defaults to `host`. A compact string list works too
(`forwards = ["8080", "5433:5432"]`), but it must sit above the first
`[section]` in the file, whereas `[[forwards]]` blocks can go anywhere.
They are reapplied on every `agv start` / `agv resume`.

You don't have to hand-edit the file: `agv forward myvm 3000:8080` adds
the same persistent forward, applying it immediately if the VM is
running and saving it either way. Add `--temporary` for a one-off tunnel
that lasts only until the VM stops.

Forwards listen on `127.0.0.1` by default. Add `bind = "0.0.0.0"` (or a
specific IP such as your tailnet address, a list, or `*`) to a
`[[forwards]]` block — or `agv forward myvm 8642 --bind 0.0.0.0` — to
expose the port on other host interfaces. That drops agv's tunnel-as-
auth-boundary, so agv warns; prefer a specific address over `0.0.0.0`.
See [`docs/config.md#forwards`](docs/config.md#forwards) for the full
reference.

**What the agent sees** — at first boot, agv writes `~/.agv/system.md`
inside the VM: a short summary of the base OS, user + sudo
capability, and every mixin that was applied (one line each). The
bundled `claude`, `gemini`, `codex`, and `openclaw` mixins each wire
their respective CLI to load it automatically (via `@`-include for
Claude / Gemini, via symlink for Codex / OpenClaw). Custom mixins can
contribute their own line by declaring `notes = [...]` in their TOML
— see [`docs/config.md`](docs/config.md#notes-mixin-authors).

**Authentication** — host environment variables drive auth where
possible. If `GH_TOKEN` / `GITHUB_TOKEN` is set when you create a VM,
the `gh` mixin runs `gh auth login --with-token` automatically. The
`claude`, `codex`, and `gemini` mixins do the same for
`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, and `GEMINI_API_KEY`
respectively, exporting each into the VM user's shell rc files. If a
key isn't set, the relevant mixin lists the manual auth step (e.g.
"run `claude /login` inside the VM") and agv prints those steps to
your terminal once provisioning succeeds. See
[`docs/repo-access.md`](docs/repo-access.md) for the full picture.

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
  rename    Rename a VM (must be stopped or suspended)
  ssh       Open an SSH session to a running VM
  gui       Open the VM's XFCE desktop in the browser (requires the gui-xfce mixin)
  cp        Copy files between the host and a running VM
  forward   Add, list, or remove host-to-guest port forwards on a running VM
  ls        List all VMs
  inspect   Show runtime status of a VM
  config    View or change VM configuration
  images    List available base images and mixins
  specs     List available hardware size presets
  resources Show host capacity and what agv has allocated
  cache     Manage the image download cache
  template  Create and manage VM templates
  backend   Migrate a VM between the QEMU and AVF backends
  init      Write a starter agv.toml to the current directory
  doctor    Check dependencies and set up SSH config integration

Options:
  -v, --verbose  Enable verbose output
  -q, --quiet    Minimal output
      --json     Output in JSON format
  -y, --yes      Assume yes for all confirmations
```

**Shutting down from inside the VM**: use `sudo poweroff` (or `sudo shutdown -h now`). `sudo halt` only halts the CPUs — it skips the ACPI poweroff event, so neither QEMU nor AVF notices the guest stopped, and `agv ls` will keep reporting the VM as `running` until you run `agv stop` from the host.

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

# Expose VM ports on your host (host:8080 → VM:8080). A `[[forwards]]`
# table can sit anywhere; a bare `forwards = [...]` list must precede
# the first [section]:
[[forwards]]
host = 8080

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

Templates are currently QEMU-only — `agv template create` refuses on AVF VMs, and clones from a template always land on the `qemu` backend even on macOS Apple Silicon where the default for `agv create` is AVF. If you want a clone on AVF, run `agv backend migrate-to-avf <name>` after the clone boots.

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
- [`docs/json-schema.md`](docs/json-schema.md) — `--json` output shapes and exit codes. The agent-facing stability contract
- [`skills/agv/SKILL.md`](skills/agv/SKILL.md) — Claude Code Skill describing how to drive `agv` from an AI agent. Copy or symlink to `~/.claude/skills/agv/` to install.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT License ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.
