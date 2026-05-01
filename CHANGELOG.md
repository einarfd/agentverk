# Changelog

All notable changes to `agv` will be documented here. This project follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Auto-suspend for idle VMs.** Set `idle_suspend_minutes = N` in the
  `[vm]` section of an `agv.toml` and the VM will save its state and
  exit QEMU after `N` minutes of confirmed idleness. Idle is the AND
  of "no interactive SSH session" and "guest 5-min load average below
  `idle_load_threshold` (default `0.2`)". Port-forward supervisors run
  `ssh -N` and don't allocate a PTY, so they never count as activity —
  config-declared forwards alone won't pin a VM as active. The watcher
  also detects host suspend (closed laptop lid) by a wall-clock gap
  between ticks and resets the idle timer on wake, so reopening the
  laptop gives the user the full configured grace window again
  instead of suspending the VM within a probe interval. Disabled by
  default (`idle_suspend_minutes = 0` or omitted). Resume with
  `agv resume`.

### Changed

- **`VmStateReport` (the JSON shape behind `agv inspect --json` and the
  lifecycle verbs) gained two fields.** `forwards` is a snapshot of the
  active port forwards for the VM — same per-entry shape as
  `agv forward --list --json`, plus a new `alive` boolean so a stale
  entry from a dead supervisor surfaces clearly. `idle_suspend` is
  `null` for VMs without auto-suspend configured; otherwise it's an
  `IdleSuspendStatus` object with the configured `minutes` /
  `load_threshold` and the watcher's `watcher_pid` /
  `watcher_alive` — distinguishing "configured + healthy",
  "configured but watcher died", and "not configured" cleanly. Per
  `docs/json-schema.md`, additions to a `0.x` schema are
  forward-compatible; existing consumers that pattern-match on
  specific keys keep working.
- **`agv forward --list --json` entries also gained `alive`.** Always
  `true` there because `--list` sweeps dead entries before
  serializing, but the field is present for shape consistency with
  the `forwards` array under `VmStateReport`.
- **`agv inspect` (human output) now shows forwards and auto-suspend.**
  A unified "Forwards" section lists every active forward with origin
  tag (`config` / `adhoc` / `auto`) and a friendly name for
  `auto_forwards`-named entries — replaces the previous per-name
  `<name> port  localhost:<port>` lines, which only covered the auto
  origin and missed config + ad-hoc forwards. An "Auto-suspend"
  section renders when `idle_suspend_minutes > 0`, showing the
  configured threshold and the watcher's PID + liveness so a dead
  watcher is visible without dropping into `--json`.

## [0.2.4] - 2026-04-29

### Added

- **`--json` on every list-like read command.** `agv images`,
  `agv specs`, `agv template ls`, `agv cache ls`,
  `agv forward --list`, and `agv doctor` now accept `--json` and
  emit a documented, schema-pinned shape (see
  `docs/json-schema.md`). Closes the last gap in the agent-facing
  read surface — every command an agent uses to discover
  available images, specs, templates, cached files, forwards, or
  host setup state now has a stable JSON contract.

### Fixed

- **`agv ssh <name> -- <cmd>` was silently broken.** clap's
  `trailing_var_arg + allow_hyphen_values` consumes a *leading*
  `--`, so the remote command words ended up as ssh options, ssh
  treated the first one as the destination, and complained
  "hostname contains invalid characters". The dispatcher now
  recovers the boundary by walking `std::env::args_os()` when
  clap discards the `--`. `agv ssh myvm -A -- ls` was unaffected
  (with at least one value before `--`, clap preserves it).
- **Race condition in the managed `ssh_config` file when two
  `agv` invocations updated it concurrently.** Two parallel
  `agv start` calls against different VMs would each
  read-modify-write `<data_dir>/ssh_config`; whichever wrote
  second would clobber the first writer's Host entry, breaking
  IDE access (`ssh <name>`) for the missing VM. `add_entry` and
  `remove_entry` in `ssh_config.rs` now hold an exclusive
  `flock(2)` advisory lock on a sibling `ssh_config.lock` file
  for the duration of the read-modify-write, serialising
  cross-process concurrent updates.

### Changed

- **Image cache downloads serialise per-image.** Two `agv create`
  calls needing the same uncached base image used to both
  download it independently (the existing PID+nanos partial-file
  scheme made this safe but wasteful). They now share an
  advisory `flock(2)` per image; the second waits, sees the file
  cached, and returns without redownloading.
- **Concurrency contract documented** in `AGENTS.md`: two `agv`
  commands against different VMs are safe to run in parallel
  (shared state is locked); two against the same VM are not (no
  per-instance locking).

## [0.2.3] - 2026-04-27

### Added

- **`agv resources`** — new command showing host capacity (total/used
  RAM, CPU count, free disk in the data dir partition) plus what agv
  has currently allocated (RAM/vCPUs across running VMs and across
  every known VM, total declared disk size). Honors `--json` for
  machine-readable output. Pulls `sysinfo` for cross-platform memory
  probing — small dep, safe (no `unsafe`), works on macOS and Linux.
- **Pre-flight capacity check on `agv create --start`** — refuses to
  boot a VM when its memory plus the memory committed to already-
  running VMs would exceed 90% of host RAM. Error message names the
  numbers and points at `agv ls` / `agv stop` for cleanup, or
  `--force` to override. Doesn't fire on `agv create` without
  `--start` (no host RAM is allocated until boot). Closes the
  resource-awareness item from `docs/agent-ergonomics.md`.
- **`agv create --if-not-exists`** — succeed silently when a VM with
  this name already exists, instead of erroring. Useful for AI agents
  that can't reliably track session state — they can run
  `agv create --if-not-exists agv-session-x --start` without first
  checking `agv ls`. Doesn't change `--start`'s behavior on an
  existing VM (use `agv start` separately if you also need it
  running).
- **`agv create --json`** — emit the new (or pre-existing, with
  `--if-not-exists`) VM's state as JSON on success: name, status,
  ssh port, memory/cpus/disk, mixins applied, any manual setup
  steps, and the instance data dir. Saves a follow-up `agv inspect`
  round trip when scripting against `agv create`. The `created`
  field distinguishes "I just created this" from "it was already
  there".
- **`agv ls --json` and `agv inspect --json`** — both emit the same
  `VmStateReport` shape as `agv create --json`. `ls --json` is a
  JSON array (one entry per VM, in the same order as the human-
  readable output); `inspect --json` is a single object. Allocated
  resource fields (memory / cpus / disk per VM) are part of the
  shape, closing the still-pending sub-item from the resource-
  awareness work.
- **`--json` on every VM-lifecycle verb**: `agv start`, `stop`,
  `suspend`, `resume`, and `rename` each emit a `VmStateReport` on
  success (same shape as `agv create --json` / `agv inspect --json`).
  `agv destroy --json` emits a small distinct `DestroyReport`
  (`{"name": "...", "destroyed": true}`) since the VM no longer
  exists. With `--json`, the verb suppresses progress chrome (treats
  itself as `--quiet`) so JSON parsing isn't broken by spinner
  residue. Saves the post-action `agv inspect` round trip an agent
  would otherwise need.
- **Distinct, documented exit codes** for the agent-relevant failure
  modes: `10` (VM/template already exists), `11` (VM/template/image
  not found), `12` (VM in wrong state for the operation), `20`
  (host capacity refused — only fires on `agv create --start` when
  the projected RAM commitment would exceed 90% of host total).
  Generic failures stay at `1`; clap usage errors at `2`. The
  resource-capacity check now returns a structured `Error::HostCapacity`
  variant instead of an untyped `anyhow!()` so the mapping is clean.
- **Labels — free-form key=value metadata on VMs.** `agv create
  --label k=v` (repeatable) attaches labels at create time; bare
  `--label foo` is shorthand for `foo=""`. Labels persist with the
  VM, surface via `agv inspect` and `agv ls --labels`, and appear
  in `--json` output as a `labels` field on `VmStateReport`.
  `agv ls --label k=v` filters to matching VMs (repeated filters
  AND together; bare-key matches any value). `agv destroy --label
  k=v` does bulk destroy by selector — refuses running VMs unless
  `--force`, and prompts (listing the matched VMs) unless `-y` or
  `--json`. agv doesn't interpret label contents; they're for
  agents tracking session ownership and for humans tagging VMs by
  purpose.
- **`docs/json-schema.md`** documents every `--json` shape and the
  exit-code table. Treats both as a stability contract over the 0.x
  series — additions OK in any minor, renames/removals only on a
  major bump. Schema-pin tests in the codebase (already shipped)
  enforce this: a removal or rename of a documented field fails CI
  loudly.

### Changed

- **Removed the unused global `--json` flag.** It was declared on
  the top-level `Cli` as `global = true` but never read by any
  command — passing `agv --json ls` accepted the flag silently
  without doing anything different. Per-command `--json` is the
  working pattern (`agv ls --json`, `agv inspect --json`,
  `agv create --json`, `agv resources --json`). Scripts that relied
  on the global form should switch to the per-command form.

## [0.2.2] - 2026-04-26

### Added

- **Top-level `notes = [...]` in `agv.toml`** now flow into the
  rendered `~/.agv/system.md`, in their own `## This VM` section
  above the mixin list. Lets a per-repo config say "this VM is for
  the foo project" without having to invent a fake mixin. Previously
  the field was parsed but discarded.
- **`optional = true` on `[[files]]`** silently skips the copy when
  the source path doesn't exist on the host, instead of erroring out
  the whole create flow. Pairs with the existing `{{VAR:-}}` template
  default for opportunistic file injection (an SSH key, a `gh` config,
  whatever the user has). Default is false so existing configs are
  unchanged.
- **`manual_steps = [...]`** — imperative instructions for the human
  invoker that agv can't automate (browser-based auth flows,
  interactive token entry). Available on mixins (top-level + per-family)
  and on the user's own `agv.toml`. Printed to the host terminal at
  the end of the first successful provision and re-printable via
  `agv inspect <vm>`. Never written into the VM — agents inside
  don't see them.
- **Host env-var-driven auth across the bundled CLI mixins.** When the
  relevant variable is set on the host at `agv create` time, agv
  configures the mixin's CLI to use it; otherwise the mixin lists the
  manual login command via `manual_steps`.
  - `gh`: `GH_TOKEN` (preferred) or `GITHUB_TOKEN` →
    `gh auth login --with-token`.
  - `claude`: `ANTHROPIC_API_KEY` → exported in `~/.bashrc` and
    `~/.zshrc`.
  - `codex`: `OPENAI_API_KEY` → same shape as claude.
  - `gemini`: `GEMINI_API_KEY` → same shape as claude.
  All four are idempotent on retry; none clobber an existing user
  configuration.
- **`ssh-key` mixin** — opt-in SSH key injection. Set
  `SSH_KEY=/path/to/private/key` in `.env` (or in the host environment)
  and `include = ["ssh-key"]` in your `agv.toml`. The mixin copies the
  key in with the right permissions and derives the matching `.pub`
  via `ssh-keygen -y`, so callers only have to point at one file. If
  `SSH_KEY` is unset the mixin is a no-op. General-purpose — works for
  any git host or non-GitHub SSH target. Replaces the previous "copy
  the SSH key inline" recipe in `docs/repo-access.md`.
- **`agv create --env-file <path>`** — explicit `.env` location that
  layers on top of the implicit `.env`-next-to-agv.toml /
  `.env`-in-cwd lookups. Useful when secrets live outside the project
  tree, or when one VM wants a different env file from another in the
  same directory. Errors out if the path doesn't exist (the implicit
  lookups stay best-effort). Host environment variables still override
  all three sources.
- **Claude Code Skill at `skills/agv/SKILL.md`** — documents how an AI
  agent should drive agv (when to use it, the five core commands,
  recipes for sandbox/auth/GUI, naming conventions, common pitfalls).
  Manual install for now: copy or symlink to `~/.claude/skills/agv/`.
  Companion `docs/agent-ergonomics.md` lists improvements that came
  out of the audit (resource awareness, idempotent create, JSON
  schema docs, distinct exit codes, labels) — likely 0.3.0 material.

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

[Unreleased]: https://github.com/einarfd/agentverk/compare/v0.2.4...HEAD
[0.2.4]: https://github.com/einarfd/agentverk/compare/v0.2.3...v0.2.4
[0.2.3]: https://github.com/einarfd/agentverk/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/einarfd/agentverk/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/einarfd/agentverk/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/einarfd/agentverk/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/einarfd/agentverk/releases/tag/v0.1.0
