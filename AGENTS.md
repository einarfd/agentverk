# CLAUDE.md

## Project overview

`agv` is a Rust CLI tool for creating and managing microVMs for AI agents. Each VM is an isolated Linux environment with SSH access, provisioned from a TOML config file.

Two hypervisor backends, picked per VM via `backend = "..."` in the config (or `--backend` on `agv create`):

- **`qemu`** — QEMU process. Cross-platform: macOS, Linux x86_64, Linux aarch64. Default everywhere except macOS on Apple Silicon. Supports `agv suspend` / `agv resume` and `idle_suspend_minutes` auto-suspend.
- **`avf`** — Apple Virtualization (`Virtualization.framework`) via a small Swift helper binary (`agv-avf-runner`). macOS Apple Silicon only. Default on that host shape. Faster cold boot than QEMU; uses raw disk images (sparse, APFS clone-on-write) rather than qcow2 overlays. Does **not** support `agv suspend` / `agv resume` or `idle_suspend_minutes` — Apple's framework does not support save/restore for Linux guests as of macOS 26. Refused at create / config-set / runtime.

## Build and test

```bash
cargo build            # Build debug binary
cargo clippy           # Lint — must pass with zero warnings
cargo test             # Run all default tests (fast, no boot)
cargo test -- --include-ignored --nocapture   # Also run slow boot tests
cargo build --release  # Release build (LTO enabled)
```

The binary is at `./target/debug/agv` (or `./target/release/agv`).

A `justfile` at the repo root chains the common multi-step workflows so
they don't need to be remembered. Install [just](https://just.systems/)
(`brew install just` or `cargo install just`) and run `just --list` to
see the recipes — `just verify` (clippy + fast tests) is the typical
pre-commit gate; `just verify-slow` adds the boot tests.

### Test policy

Tests fall into three categories. Pick the right one when adding a new test:

**1. Always-on, no external tools** — runs on every `cargo test`, no skip logic
- **Where:** unit tests inside `src/*.rs`, and `tests/cli_test.rs`
- **What:** pure logic (parsing, formatting, state machines), CLI argument parsing,
  error message shapes, help output. Anything that does not touch a real subprocess
  or download.
- **Examples:** `interactive::tests::*`, `vm::instance::tests::*`,
  `tests/cli_test.rs::ssh_help_succeeds`

**2. Runtime-skip integration** — runs on every `cargo test`, but skips if a tool is missing
- **Where:** top-level integration tests in `tests/*.rs` that are NOT marked `#[ignore]`
- **What:** uses `qemu-img`, `mkisofs`/`hdiutil`, briefly spawns a `qemu-system-*` process
  with a fake/empty disk, etc. Fast (under ~10 seconds per test). Does not need network
  and does not boot a guest OS.
- **Skip mechanism:** call a helper like `qemu_available()`/`qemu_img_available()` at
  the top of the test, `eprintln!` and `return` if missing. Do not panic on missing
  tools — these tests must never fail in environments without them.
- **Examples:** `tests/qemu_test.rs::qemu_start_and_force_stop`,
  `tests/create_test.rs::create_without_start`

**3. Slow boot tests** — opt-in, marked `#[ignore]`
- **Where:** `tests/create_test.rs` (and similar). Marked with
  `#[ignore = "downloads a real cloud image and boots a VM — slow"]`
- **What:** downloads a real cloud image (~330 MB), boots a guest OS, runs full
  provisioning. Takes minutes per test.
- **Run with:** `cargo test -- --include-ignored --nocapture`
- **Conventions:** still call the runtime-skip helpers (so they no-op gracefully if
  tools are missing). Use VM names prefixed `_test-` and call `cleanup(name)` at the
  start and end.
- **Examples:** `create_with_start_and_provision`, `suspend_and_resume_preserves_state`,
  `provision_failure_then_retry_resumes`, `fedora_base_boots_and_provisions`,
  `auto_forwards_end_to_end`

**Decision rule:**
- Pure logic with no external state → category 1
- Touches `qemu-img`, briefly spawns QEMU, or generates a seed ISO, but no network and < 10s → category 2
- Downloads a cloud image or boots a guest OS → category 3

## Architecture

- **Entry point**: `src/main.rs` — tracing init, CLI parse, error display
- **Command dispatch**: `src/lib.rs` — matches CLI subcommand and calls into modules
- **CLI definition**: `src/cli.rs` — clap derive structs for all commands and flags
- **Config**: `src/config.rs` — serde structs for `agv.toml` parsing, inheritance resolution
- **Errors**: `src/error.rs` — `thiserror` enum with all error variants
- **VM lifecycle**: `src/vm/mod.rs` — orchestrates create/start/stop/destroy, file copy, provisioning
- **Instance state**: `src/vm/instance.rs` — on-disk state, status reconciliation
- **Backend dispatch**: `src/vm/backend.rs` — `VmBackend` trait + `LocalQemuBackend` / `LocalAvfBackend` impls. `for_instance(inst)` reads `<inst>/config.toml` to pick which one provision/start/stop/suspend dispatch through.
- **QEMU**: `src/vm/qemu.rs` — process spawning and QMP protocol
- **AVF runner (Swift)**: `swift/avf-runner/Sources/avf-runner/main.swift` — per-VM `VZVirtualMachine` supervisor spawned by the AVF backend, controlled via line-delimited JSON-RPC on a unix socket. `swift/avf-runner/Sources/AvfRunnerCore/` holds pure-logic helpers (DHCP-lease lookup) split out so they're unit-testable without main.swift's top-level boot code.
- **AVF backend wiring**: `src/vm/backend.rs::LocalAvfBackend` — spawns the runner, talks RPC, owns the runner pid file (`<inst>/avf-runner.pid`), MAC sidecar (`<inst>/avf-mac`), machine-id sidecar (`<inst>/avf-machine-id`), EFI variable store (`<inst>/avf-efi-vars.bin`), and snapshot file (`<inst>/avf-snapshot.bin`).
- **qcow2 conversion**: `src/qcow2.rs` — macOS-only pure-Rust qcow2 → sparse raw converter (AVF can't read qcow2 directly). Verified byte-identical to `qemu-img convert -O raw`.
- **Raw cache**: `src/raw_cache.rs` — macOS-only cache of qcow2 → raw conversions under `<cache>/<basename>.qcow2.raw`. First AVF create from a base pays the conversion; later creates clone via `cp -c` (APFS `clonefile(2)`) for zero-byte instant per-VM disks.
- **System info (`~/.agv/system.md`)**: `src/vm/system_info.rs` — renders a short markdown file written into the VM after provisioning so agents inside can discover applied mixins and non-obvious wiring. Notes come from each mixin's optional `notes = [...]` field.
- **Port forwarding runtime**: `src/vm/forwarding.rs` — add/list/stop on a running VM, spawns supervisors, persists to `<instance>/forwards.toml`
- **Forward supervisor**: `src/forward_daemon.rs` — long-running loop around `ssh -N -L`, respawns on exit. Invoked as the hidden CLI subcommand `__forward-daemon`.
- **Port forwarding types**: `src/forward.rs` — `ForwardSpec` parser (`HOST[:GUEST]`), active-forwards state file I/O, supervisor `kill_supervisor`/`kill_all_and_clear` helpers
- **Cloud-init**: `src/vm/cloud_init.rs` — seed image generation (user setup, SSH keys, hostname only)
- **SSH**: `src/ssh.rs` — shells out to system `ssh`/`scp` for sessions, commands, and file copy
- **Images**: `src/image.rs` — download, cache, checksum, qcow2 overlays
- **Image registry**: `src/images/` — built-in and user-defined image/mixin catalogue (TOML files)
- **Specs**: `src/specs/` — hardware size presets (small/medium/large/xlarge)
- **Init**: `src/init.rs` — `agv init` command, embeds example configs via `include_str!`
- **Interactive**: `src/interactive.rs` — y/n/e/a/q prompting for `--interactive` mode
- **Doctor**: `src/doctor.rs` — `agv doctor` dependency checker with platform-specific hints
- **SSH config**: `src/ssh_config.rs` — managed `~/.ssh/config` integration for IDE/SSH access by VM name
- **Templates**: `src/template.rs` — `{{VAR}}` expansion in config values, `.env` file loading
- **Directories**: `src/dirs.rs` — XDG-compliant data paths, `AGV_DATA_DIR` override

## Key design decisions

- **File injection uses SCP, not cloud-init.** `[[files]]` are copied via `ssh::copy_to()` after SSH is ready, with explicit `mkdir -p` for parent directories. Cloud-init `write_files` was removed because it silently failed and corrupted home directory ownership.
- **`agv ssh` passes all args after the VM name to ssh.** Uses clap `trailing_var_arg` — everything before `--` becomes ssh options (e.g. `-A`, `-L`), everything after `--` is the remote command.
- **Cloud-init seed only handles user creation, SSH keys, and hostname.** All file and software setup happens after SSH is ready, via the setup/provision/file-copy flow.
- **ISO creation is platform-specific.** macOS uses built-in `hdiutil`, Linux uses `mkisofs`/`genisoimage`. Split with `#[cfg(target_os = "macos")]`.
- **Managed SSH config for IDE integration.** `ssh_config.rs` maintains `<data_dir>/ssh_config` with Host entries for running VMs. Updated automatically on start/stop/destroy. Users add an Include once via `agv doctor --setup-ssh`.
- **`agv cp` wraps scp** with VM-aware syntax — `:path` marks a path inside the VM.
- **`agv forward` uses SSH `-L` tunnels with an agv-spawned supervisor.** Each forward is its own long-lived child process running a loop around `ssh -N -L PORT:localhost:PORT` so it survives transient SSH failures (sshd hiccup, brief network blip). The supervisor is detached into its own process group; stopping a forward sends `SIGTERM` to that group, killing both the supervisor and any in-flight ssh. SSH (rather than QEMU hostfwd) is required because user-mode hostfwd cannot reach guest services bound to `127.0.0.1` — SSH resolves `localhost` from inside the guest. Add/list/stop subcommands mutate the live set; runtime changes are ephemeral and wiped on next start/resume. Persistent forwards are declared in config (`forwards = [...]` or `agv config set --forwards`) and reapplied on every start/resume. Host<->guest specs use the form `HOST[:GUEST][/PROTO]`. State tracked in `<instance>/forwards.toml` with origin (`config`/`adhoc`) and supervisor `pid` so `--list` and reconcile can distinguish and sweep dead entries.
- **`agv suspend` / `agv resume` use QEMU savevm/loadvm.** State is stored as a snapshot inside the qcow2 disk (no extra files). Suspend uses HMP `savevm` via QMP `human-monitor-command`, then exits QEMU; resume passes `-loadvm agv-suspend` to QEMU on start.
- **AVF suspend/resume is refused, not wired.** Apple Virtualization framework's `saveMachineStateTo` succeeds for Linux guests, but `restoreMachineStateFrom` always fails with the misleading `VZErrorDomain Code=12 "permission denied"` — reproduced both cross-process and same-process, with canonicalized paths (`realpath(3)` resolves macOS firmlinks that `URL.resolvingSymlinksInPath()` doesn't), minimal device list, and persisted MAC + machine identifier. Apple's own sample, Tart, UTM, and Lima all gate save/restore on macOS guests. Three refusal sites: (1) `agv create` / TOML resolve rejects `backend = "avf"` + `idle_suspend_minutes > 0` in `build_from_cli`; (2) `agv config set --idle-suspend-minutes` rejects the same combo for an AVF VM; (3) `agv suspend` bails before tearing down forwards/watcher with a clear error, and the runner's `suspend` RPC refuses as a defense-in-depth final line. The runner still keeps `restoreAndResume` wired (with a `validateSaveRestoreSupport()` pre-check) so any future macOS release that lifts the restriction will light it up automatically. The `.cached` + `.full` disk attachment is kept regardless — `.automatic` caused ext4 corruption under sustained small-file I/O (Lima PR #2026 / Tart / UTM PR #5919 for the same workaround). Removing the virtio-rng entropy device is also still required to avoid a separate device-list rejection.
- **AVF guest IP via the host DHCP lease file.** AVF's NAT bridge runs `bootpd`, which writes leases to `/var/db/dhcpd_leases`. The runner reads that file on every `status` RPC, keyed by hostname first (cloud-init's `local-hostname` matches the VM name) and falling back to MAC. When a hostname has been used across multiple VM incarnations the file accumulates several matching blocks — `LeaseLookup` picks the one with the highest `lease=` timestamp, not the last in file order (bootpd writes by IP, not by recency).
- **AVF instance disks are independent of the cache.** `provision_disk` runs `ensure_cached_raw` (convert once, write to `<cache>/<basename>.qcow2.raw`) then `clone_to` (`cp -c`, i.e. `clonefile(2)`) into `<inst>/disk.raw`. APFS reference-counts the underlying extents; deleting either side never affects the other, and writes to the clone do not propagate to the cache. The cache is a speedup, not a runtime dependency.
- **`agv backend migrate-to-avf` flips a stopped VM in place.** Converts the per-instance qcow2 to a sparse raw, sets `backend = "avf"` in the saved config, bumps the cloud-init instance-id (to force netplan to re-apply for the new NIC name on the AVF NAT). The original qcow2 stays by default for one-step rollback; pass `--delete-qcow2` to remove it during migration, or run `agv backend cleanup <name>` afterwards to reclaim it once the AVF boot is verified.
- **`agv backend cleanup <name>` sweeps previous-backend residue.** Looks at the VM's recorded backend and removes files belonging to the OTHER backend (`disk.qcow2` etc. on an AVF VM, `disk.raw` + `avf-*` sidecars on a QEMU VM). Refuses while the VM is running or has a live host process. Bidirectional — direction is implied by the current `backend` field, not specified.
- **Default backend flips by host.** `config::default_backend()` returns `"avf"` on macOS aarch64, `"qemu"` everywhere else. Existing VMs are unaffected because each instance config records its own backend; the default only feeds into new VMs created without `--backend` or a TOML override.
- **Provision state is tracked per phase + step index.** `<instance>/provision_state` (TOML) records phase (`ssh_wait`/`files`/`setup`/`provision`/`complete`) and the next step index. On first-boot failure, the VM is marked `broken` but QEMU is left running so the user can SSH in to debug. `agv start --retry` resumes from the saved phase/index, skipping completed steps. Legacy VMs with the old `provisioned` touch file are auto-detected as `Complete`.
- **Interactive provisioning (`-i/--interactive` on `create` and `start`).** Prompts before each file copy, setup step, and provision step with `y/n/e/a/q`. Edit (`e`) is runtime-only — does not modify the saved config. Implemented in `src/interactive.rs` with `prompt_step_io` for testability.
- **New `[vm]` fields go in `agv config set` too.** Anything in `VmConfig` (`src/config.rs`) that's safe to change between sessions should also be wired through `ConfigSetArgs` (`src/cli.rs`) and `vm::config_set` (`src/vm/mod.rs`). Otherwise users have to hand-edit `<instance>/config.toml`. The pattern is: add a flag, thread it through the function signature, set the field on the loaded `ResolvedConfig`, and print an old → new line in the dispatcher (`src/lib.rs`).
- **Inline `run` scripts get `set -e` injected before bash sees them.** `provision.rs::with_set_e` prepends `set -e\n` to every inline setup/provision `run` string. Without it, multi-line scripts silently swallow mid-script failures because bash's exit status is the last command's. Mixins that want stricter modes (`-u`, `pipefail`) still declare them themselves; this is just the safe default. Script-file steps (`script = "..."`) are unaffected — those carry their own shebang.
- **`~/.agv/system.md` for in-VM agent discoverability.** Written once at the end of first-boot provisioning via SSH (base64-encoded payload to sidestep shell quoting). Contains OS family, user + sudo capability, and one bullet per applied mixin (with its `notes = [...]` line if declared, else just the name). Each agent-CLI mixin wires its tool to pick the file up automatically: `claude` and `gemini` append a one-line `@~/.agv/system.md` pointer to `~/.claude/CLAUDE.md` / `~/.gemini/GEMINI.md` (both tools resolve `@<path>` as a file include); `codex` and `openclaw` have no include syntax, so they symlink `~/.codex/AGENTS.md` / `~/.openclaw/workspace/AGENTS.md` to `~/.agv/system.md`. All four are idempotent on retry and skip silently if a user-authored file is already there.

## Runner ↔ agv wire-protocol versioning

The agv (Rust) ↔ agv-avf-runner (Swift) JSON contract — both the spawn-time `RunnerConfig` and the line-delimited control-socket RPC — carries a `runner_protocol_version` field. The runner refuses to boot if the version in its config doesn't match its own compiled-in constant, with a clear `reinstall` hint. The two source-of-truth constants must match exactly:

- Rust: `RUNNER_PROTOCOL_VERSION` in `src/vm/backend.rs`
- Swift: `RUNNER_PROTOCOL_VERSION` in `swift/avf-runner/Sources/avf-runner/main.swift`

**Bump both constants — in the same commit — when:**
- Adding, removing, or renaming a field in the runner config or control-socket request/response.
- Adding, removing, or renaming a control-socket op.
- Changing the *semantics* of an existing op or field even if the wire shape is unchanged. Example: `stop` used to only do ACPI shutdown and now escalates to SIGKILL — that's a semantic change agv depends on, so it would have been a bump.
- Fixing a runner bug that changes what agv observes (e.g. the path-canonicalization change to make restore work).
- Adding a new capability the runner advertises.

**Don't bump for** pure refactors with no observable wire or behavioural change.

Increment by 1 each time. There is no semver and no compatibility range — strict equality, install-skew is the only failure mode this guards against, and agv + runner are expected to ship together (`just install` for source installs; release tarballs bundle both binaries). `agv-avf-runner --version` prints the protocol version directly (no separate binary version), which is what `agv doctor` checks against to surface mismatches early.

Forward-compatibility caveat: serde and Swift's `JSONDecoder` both ignore unknown JSON fields by default, so an older runner reading a newer agv's config will silently drop fields rather than reject them. That's why we have the explicit version field — wire-shape tolerance is not enough to catch behavioural drift.

## Conventions

- **Error handling**: `anyhow::Result` for application code, `thiserror` for library error types in `error.rs`
- **Async runtime**: Tokio — all I/O operations are async
- **Lints**: `clippy::pedantic` is enabled; all warnings must be fixed before committing
- **Suppressing lints**: use `#[expect(clippy::foo, reason = "...")]` instead of `#[allow(clippy::foo)]`. The `clippy::allow_attributes` and `clippy::allow_attributes_without_reason` lints enforce this. `expect` requires a reason and warns if the underlying lint is no longer firing, so dead suppressions get caught automatically.
- **Unsafe**: Forbidden (`unsafe_code = "forbid"` in Cargo.toml)
- **Edition**: Rust 2024

## Project structure

- `docs/` — config reference (`config.md`), repo access guide (`repo-access.md`), remote IDE setup (`remote-ide.md`)
- `examples/` — ready-to-use `agv.toml` files for Claude, Gemini, Codex, OpenClaw, repo checkout
- `.github/workflows/` — CI (clippy + tests) and release (cross-platform binary builds)
- `install.sh` — curl-pipe-sh installer that downloads the right binary and runs `agv doctor`

## VM state storage

`~/.local/share/agv/` (XDG-compliant, same on all platforms). Override with `AGV_DATA_DIR`.

Instance state lives in `instances/<name>/`. Files common to both backends: `seed.iso`, `id_ed25519`, `id_ed25519.pub`, `config.toml`, `status`, `serial.log`, `provision.log`, `error.log`, `provisioned`, `provision_state`, `idle_watcher.pid`, `idle_watcher.log` (the watcher's redirected stderr — probe/idle decisions and suspend attempts; truncated per watcher spawn), `forwards.toml` (present when forwards are active; lists each forward's spec, origin, and supervisor PID), `<name>_port` files (one per declared `[auto_forwards.<name>]`, holding the auto-allocated host port for the VM's lifetime).

Backend-specific files:
- **QEMU**: `disk.qcow2`, `pid`, `ssh_port`, `qmp.sock`, `efi-vars.fd` (aarch64 only).
- **AVF**: `disk.raw`, `avf-runner.pid`, `avf-control.sock`, `avf-runner-config.json`, `avf-runner.log`, `avf-mac`, `avf-machine-id`, `avf-efi-vars.bin`, `avf-snapshot.bin` (only while suspended).

The data dir also contains `ssh_config` — a managed SSH config file with Host entries for running VMs (see `ssh_config.rs`).

VM templates live in `templates/` as paired `<name>.qcow2` + `<name>.toml` files.

## VM statuses

`creating` | `configuring` | `running` | `stopped` | `suspended` | `broken`

A `broken` VM can only be destroyed (or unblocked with `agv start --retry`). If a `running` VM's host process is stale, it auto-transitions to `stopped`. A `suspended` QEMU VM has its full state saved to a snapshot inside `disk.qcow2` (named `agv-suspend`); resume restarts QEMU with `-loadvm`. A `suspended` AVF VM has its state saved to `<inst>/avf-snapshot.bin`; resume re-spawns the runner with `restore_on_boot: true`, which calls `restoreMachineStateFrom` and removes the snapshot once the VM is back to `running`.

## Concurrency contract

- **Two `agv` commands against different VMs are safe to run in parallel.** Shared state (the managed `<data_dir>/ssh_config`, the image cache) is protected by `flock(2)` advisory locks via `src/locks.rs`, applied around the read-modify-write critical sections in `ssh_config::add_entry` / `remove_entry` and around `image::ensure_cached`'s download path.
- **Two `agv` commands against the same VM are not safe.** agv doesn't lock individual instance directories — running, e.g., `agv start myvm` and `agv stop myvm` simultaneously is undefined. Single-VM ops are expected to be one-at-a-time.
- **Lockfiles** are siblings of the protected file: `<data_dir>/ssh_config.lock`, `<data_dir>/cache/images/<file>.lock`. Zero bytes; not garbage-collected (cost is negligible).
- The lock acquire is delegated to `tokio::task::spawn_blocking` so a contended lock doesn't park the async runtime's worker thread. The OS releases the lock when the holding fd closes (so panic-safe via `Drop`).
