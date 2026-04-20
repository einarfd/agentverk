# Changelog

All notable changes to `agv` will be documented here. This project follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **OS-family schema for mixins.** Base images now declare `os_family`
  (`"debian"`, `"fedora"`, `"alpine"`, …) and mixins can declare which
  families they support, either explicitly via `supports = [...]` or
  implicitly via the keys of `[os_families.<name>]` sections. Mixins with
  per-family steps put them under `[os_families.<name>]` and the resolver
  picks the section matching the base image. Distro-agnostic mixins
  keep the existing top-level shape unchanged. See `docs/config.md`
  for the full schema.
- **Fedora 43 base image.** `agv create --image fedora-43` boots a
  Fedora Cloud Base (Generic) VM. The `devtools` mixin now ships a
  fedora variant (using `dnf` instead of `apt-get`), so
  `agv create --image fedora-43 --include devtools <name>` works
  out of the box.

### Changed

- Root image configs (in `src/images/`) now require `[base] os_family`.
  The bundled `ubuntu-24.04` and `debian-12` declare `os_family = "debian"`.
  Saved instance `config.toml` files from v0.1.0 default to `"debian"`
  on load, so existing VMs keep working.
- Apt-only mixins (`gh`, `nodejs`, `rust`, `zsh`, `oh-my-zsh`) now declare
  `supports = ["debian"]`. Users trying them with a non-debian base get a
  clean config-resolve-time error instead of mid-provisioning failures.
  Fedora / alpine ports of these mixins are follow-up work.

### Known limitations

- Alpine support is not yet shipped. Alpine's cloud images are UEFI-only
  on both x86_64 and aarch64, but agv currently only configures UEFI
  firmware for aarch64. Adding x86_64 UEFI is a prerequisite and will
  arrive in a follow-up.

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
