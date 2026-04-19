# Changelog

All notable changes to `agv` will be documented here. This project follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-04-19

Initial public release. `agv` creates and manages QEMU/KVM microVMs for AI
coding agents on macOS (Apple Silicon) and Linux (x86_64, aarch64).

### Added

- **VM lifecycle**: `create`, `start`, `stop`, `suspend`, `resume`, `destroy`,
  and `rename` — each VM is an isolated Linux environment with its own disk,
  cloud-init seed, and unique ED25519 SSH keypair.
- **`agv ssh` / `agv cp`**: SSH into a running VM or copy files between host
  and guest. `agv ssh <name>` accepts trailing args forwarded to `ssh`
  (`-A`, `-L`, etc.).
- **Managed SSH config integration**: `agv doctor --setup-ssh` adds an
  `Include` directive to `~/.ssh/config` pointing at a file `agv` maintains,
  so VS Code, JetBrains, and plain `ssh` can reach any running VM by name.
- **Port forwards**: declare persistent forwards in config (`forwards = [...]`)
  or manage them at runtime with `agv forward add/list/stop`. Each forward
  runs as a respawning SSH tunnel supervisor.
- **Config file**: TOML-based, supports image inheritance (`base.from`),
  reusable mixins (`include = [...]`), hardware size presets (`spec`), file
  injection (`[[files]]`), root setup steps (`[[setup]]`), user provisioning
  (`[[provision]]`), and `{{VAR}}` template expansion with `.env` loading.
- **`run = [...]` array form** on `[[setup]]` / `[[provision]]` blocks — one
  entry per step, saves repeating the block header for related commands.
- **Interactive provisioning** (`-i/--interactive`): step through file copies,
  setup, and provision commands with y/n/e/a/q — edit commands inline before
  they run.
- **Provision state tracking**: on failure, the VM is marked `broken` and left
  running so you can SSH in to debug. `agv start --retry` resumes from the
  saved phase/step.
- **Templates**: `agv template create <vm> <name>` turns a provisioned VM into
  a reusable base image; `agv create --from <template>` stamps a thin clone.
- **Image cache**: `agv cache ls/clean`. Cloud images are fetched over HTTPS
  and SHA-256 verified before use.
- **Suspend / resume**: `agv suspend` saves full VM state (RAM + devices) to a
  qcow2 snapshot; `agv resume` boots from it with `-loadvm`.
- **`agv init`**: writes a starter `agv.toml`. Templates available for
  `claude`, `gemini`, `codex`, and `openclaw`.
- **`agv doctor`**: dependency checker with platform-specific install hints.
- **Built-in mixins**: `devtools`, `nodejs`, `rust`, `uv`, `docker`, `gh`,
  `zsh`, `oh-my-zsh`, `claude`, `gemini`, `codex`, `openclaw`.
- **Output modes**: `--json` for machine-readable output, `-v/--verbose` and
  `-q/--quiet` for log verbosity, `-y/--yes` to skip confirmations.

### Security

- Each VM has a unique SSH keypair, deleted on `agv destroy`.
- Downloaded cloud images are SHA-256 verified before use.
- Port forwards bind on `127.0.0.1` only.
- Managed SSH config integration is opt-in (`agv doctor --setup-ssh`).

See [`SECURITY.md`](SECURITY.md) for scope and reporting instructions.

[Unreleased]: https://github.com/einarfd/agentverk/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/einarfd/agentverk/releases/tag/v0.1.0
