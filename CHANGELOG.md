# Changelog

All notable changes to `agv` will be documented here. This project follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.1] - 2026-04-23

### Added

- **`devtools` mixin now includes `ripgrep`, `jq`, `fd`, `tree`,
  `shellcheck`, `sqlite3`, and `tmux`.** All seven are small,
  distro-packaged, and carry their weight for both humans (shell
  sessions, terminal multiplexing) and agents (searching code,
  parsing JSON, poking SQLite). On Debian, `fd-find` installs as
  `fdfind` (namespace conflict with an old init replacement) — the
  mixin symlinks `/usr/local/bin/fd` so the canonical name works.
  On Fedora the binary is already `fd`.
- **`~/.agv/system.md` — a short, token-cheap summary of the VM for
  agents inside it.** Written at the end of first-boot provisioning
  with the base OS family, the user and its passwordless-sudo
  capability, and every mixin that was applied. Each mixin describes
  itself in one short line via a new optional `notes = [...]` field;
  mixins without notes still show up by name. Each of the four
  bundled agent-CLI mixins also wires its tool to pick the file up
  automatically: `claude` and `gemini` append a one-line
  `@~/.agv/system.md` pointer to `~/.claude/CLAUDE.md` and
  `~/.gemini/GEMINI.md` (both tools resolve `@<path>` as a file
  include); `codex` and `openclaw` have no file-include syntax, so
  they symlink `~/.codex/AGENTS.md` and
  `~/.openclaw/workspace/AGENTS.md` to `~/.agv/system.md`. All four
  are idempotent on retry and skip silently when a user-authored
  file is already there. All bundled mixins ship `notes`: install
  contents for the toolchain mixins (`devtools`, `gh`, `nodejs`,
  `rust`, `uv`, `zsh`, `oh-my-zsh`, `claude`, `codex`, `gemini`,
  `openclaw`) and state-level details for the ones where it matters
  (`docker` service/group, `gui-xfce` host-side `agv gui` command,
  `zsh` default-shell change).

### Fixed

- **`docker` mixin did not start the Docker service on Fedora.** The
  install step ran `get.docker.com` and added the user to the `docker`
  group but relied on post-install auto-start, which is a
  Debian/Ubuntu-specific convention. Fedora's `dnf install` leaves
  services disabled. The setup step now also runs
  `systemctl enable --now docker`, which is a no-op on Debian.
- **`agv suspend` raced with QEMU's pidfile cleanup on fast systems.**
  QEMU removes its own pidfile on exit, and the QMP `quit` command is
  immediate — so reading the pidfile after `quit` sometimes returned
  ENOENT, surfacing as "failed to read PID file ..." from
  `agv suspend`. The pid is now captured before `quit` is sent, so
  the exit-poll has a valid pid in hand either way. `agv stop` got
  the same reordering for consistency, though its ACPI-based
  `system_powerdown` is slow enough that the race was essentially
  unreachable there.

## [0.2.0] - 2026-04-22

### Changed

- **Port forward specs no longer accept a `/proto` suffix.** The `/tcp` and
  `/udp` suffixes were accepted historically but never functional — every
  forward was tunneled over TCP regardless. The `Proto` enum and proto
  field are gone from `ForwardSpec` / `ActiveForward`; forwards.toml state
  files from older agv versions still load (the now-unknown `proto` field
  is silently ignored by serde). A legacy `53/udp` in a config file now
  fails at parse time with a message explaining to drop the suffix.
- **Colored output** in `agv doctor` and `install.sh`: pass/fail marks and
  status lines are tagged green/yellow/red when stdout is a TTY. Respects
  the `NO_COLOR` standard and strips codes automatically when piped, so
  CI logs and files stay clean.
- **`agv ls` now surfaces why** an instance shows `?` in the image column:
  `agv -v ls` logs the underlying `config::load_resolved` error via
  `tracing::debug!` instead of silently swallowing it.

### Added

- **`gui-xfce` mixin + `agv gui` command.** Opt-in browser-based
  XFCE desktop, declared by `include = ["gui-xfce"]`. The guest runs
  TigerVNC (X server + XFCE session) + noVNC (HTML5 client) +
  websockify, all bound to `127.0.0.1`. The port rides a
  `[auto_forwards.gui]` SSH tunnel, gated by the VM's ed25519 key, so
  the VNC server runs with `-SecurityTypes None` — no password ever
  appears in a URL, browser history, or localStorage. `agv gui <vm>`
  just opens the browser at
  `http://127.0.0.1:<port>/vnc.html?autoconnect=1&resize=scale`.
  Single step; same UX on macOS, Linux, Windows. Supports debian and
  fedora families.
- **`[auto_forwards.<name>]` schema** for mixins. A mixin can declare
  "I need a tunnel to guest port X under a stable name" without picking a
  host port, and agv allocates one at VM start, writes it to
  `<instance>/<name>_port`, and spawns an SSH-tunnel supervisor that
  persists for the VM's lifetime. Mirrors the SSH port pattern so
  protocols like RDP / VNC / a computer-use control plane become mixin
  material without any additional plumbing changes. Surfaces in
  `agv inspect` and `agv forward --list` (Origin: `auto`).
- **`--image` shorthand aliases.** `--image ubuntu`, `--image debian`, and
  `--image fedora` now resolve to the current canonical versions
  (`ubuntu-24.04`, `debian-12`, `fedora-43`). Aliases are pure CLI sugar
  — the saved instance config records the concrete URL, so there's no
  lingering ambiguity. For script stability, prefer the canonical names;
  aliases will move when a newer release ships.
- **OS-family schema for mixins.** Base images now declare `os_family`
  (`"debian"`, `"fedora"`, `"alpine"`, …) and mixins can declare which
  families they support, either explicitly via `supports = [...]` or
  implicitly via the keys of `[os_families.<name>]` sections. Mixins with
  per-family steps put them under `[os_families.<name>]` and the resolver
  picks the section matching the base image. Distro-agnostic mixins
  keep the existing top-level shape unchanged. See `docs/config.md`
  for the full schema.
- **Fedora 43 base image.** `agv create --image fedora-43` boots a
  Fedora Cloud Base (Generic) VM, verified to boot and SSH cleanly.
- **Fedora-ready mixins.** `devtools`, `gh`, `nodejs`, `rust`, `zsh`,
  and (transitively) `oh-my-zsh` now ship `[os_families.fedora]`
  sections alongside the existing debian ones, so every config example
  in `examples/` works unchanged against `fedora-43`. `uv` declares
  `supports = ["debian", "fedora"]` (its install script downloads a
  glibc binary, so musl/Alpine would silently fail).

### Changed

- Root image configs (in `src/images/`) now require `[base] os_family`.
  The bundled `ubuntu-24.04` and `debian-12` declare `os_family = "debian"`.
  Saved instance `config.toml` files from v0.1.0 default to `"debian"`
  on load, so existing VMs keep working.
- `oh-my-zsh` now depends on the `zsh` mixin via `[base] include = ["zsh"]`
  instead of duplicating zsh's install + `chsh` steps. Family support
  is inherited from zsh automatically.

### Fixed

- **UTF-8 panic in provision step labels.** `step_label` sliced by byte
  count when truncating long `run` strings, which panicked when byte 40
  fell inside a multi-byte char (emoji, accented latin, CJK, …). Truncates
  by character now.

### Documentation

- README gained a *Desktop / GUI access* section, `examples/gui/` ships a
  ready-to-use XFCE desktop config, and `docs/config.md` ↔
  `docs/remote-ide.md` now cross-link.
- New `CONTRIBUTING.md` covering the build/lint/test expectations and the
  slow-test policy.

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

[Unreleased]: https://github.com/einarfd/agentverk/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/einarfd/agentverk/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/einarfd/agentverk/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/einarfd/agentverk/releases/tag/v0.1.0
