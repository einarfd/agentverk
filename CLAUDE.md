# CLAUDE.md

## Project overview

`agv` is a Rust CLI tool for creating and managing QEMU/KVM microVMs for AI agents. Each VM is an isolated Linux environment with SSH access, provisioned from a TOML config file.

## Build and test

```bash
cargo build            # Build debug binary
cargo clippy           # Lint — must pass with zero warnings
cargo test             # Run all default tests (fast, no boot)
cargo test -- --include-ignored --nocapture   # Also run slow boot tests
cargo build --release  # Release build (LTO enabled)
```

The binary is at `./target/debug/agv` (or `./target/release/agv`).

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
  `provision_failure_then_retry_resumes`

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
- **QEMU**: `src/vm/qemu.rs` — process spawning and QMP protocol
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
- **Directories**: `src/dirs.rs` — platform-specific paths (macOS/Linux)

## Key design decisions

- **File injection uses SCP, not cloud-init.** `[[files]]` are copied via `ssh::copy_to()` after SSH is ready, with explicit `mkdir -p` for parent directories. Cloud-init `write_files` was removed because it silently failed and corrupted home directory ownership.
- **`agv ssh` passes all args after the VM name to ssh.** Uses clap `trailing_var_arg` — everything before `--` becomes ssh options (e.g. `-A`, `-L`), everything after `--` is the remote command.
- **Cloud-init seed only handles user creation, SSH keys, and hostname.** All file and software setup happens after SSH is ready, via the setup/provision/file-copy flow.
- **ISO creation is platform-specific.** macOS uses built-in `hdiutil`, Linux uses `mkisofs`/`genisoimage`. Split with `#[cfg(target_os = "macos")]`.
- **Managed SSH config for IDE integration.** `ssh_config.rs` maintains `<data_dir>/ssh_config` with Host entries for running VMs. Updated automatically on start/stop/destroy. Users add an Include once via `agv doctor --setup-ssh`.
- **`agv cp` and `agv forward` wrap scp/ssh** with VM-aware syntax. `cp` uses `:path` prefix for VM paths; `forward` uses `local[:remote]` port specs. Both check VM status before connecting.
- **`agv suspend` / `agv resume` use QEMU savevm/loadvm.** State is stored as a snapshot inside the qcow2 disk (no extra files). Suspend uses HMP `savevm` via QMP `human-monitor-command`, then exits QEMU; resume passes `-loadvm agv-suspend` to QEMU on start.
- **Provision state is tracked per phase + step index.** `<instance>/provision_state` (TOML) records phase (`ssh_wait`/`files`/`setup`/`provision`/`complete`) and the next step index. On first-boot failure, the VM is marked `broken` but QEMU is left running so the user can SSH in to debug. `agv start --retry` resumes from the saved phase/index, skipping completed steps. Legacy VMs with the old `provisioned` touch file are auto-detected as `Complete`.
- **Interactive provisioning (`-i/--interactive` on `create` and `start`).** Prompts before each file copy, setup step, and provision step with `y/n/e/a/q`. Edit (`e`) is runtime-only — does not modify the saved config. Implemented in `src/interactive.rs` with `prompt_step_io` for testability.

## Conventions

- **Error handling**: `anyhow::Result` for application code, `thiserror` for library error types in `error.rs`
- **Async runtime**: Tokio — all I/O operations are async
- **Lints**: `clippy::pedantic` is enabled; all warnings must be fixed before committing
- **Unsafe**: Forbidden (`unsafe_code = "forbid"` in Cargo.toml)
- **Edition**: Rust 2024

## Project structure

- `docs/` — config reference (`config.md`), repo access guide (`repo-access.md`), remote IDE setup (`remote-ide.md`)
- `examples/` — ready-to-use `agv.toml` files for Claude, Gemini, Codex, OpenClaw, repo checkout
- `.github/workflows/` — CI (clippy + tests) and release (cross-platform binary builds)
- `install.sh` — curl-pipe-sh installer that downloads the right binary and runs `agv doctor`

## VM state storage

- macOS: `~/Library/Application Support/agv/`
- Linux: `~/.local/share/agv/`

Instance state lives in `instances/<name>/` with files: `disk.qcow2`, `seed.iso`, `id_ed25519`, `id_ed25519.pub`, `config.toml`, `status`, `pid`, `ssh_port`, `qmp.sock`, `serial.log`, `provision.log`, `error.log`, `provisioned`, `efi-vars.fd` (aarch64 only).

The data dir also contains `ssh_config` — a managed SSH config file with Host entries for running VMs (see `ssh_config.rs`).

VM templates live in `templates/` as paired `<name>.qcow2` + `<name>.toml` files.

## VM statuses

`creating` | `configuring` | `running` | `stopped` | `suspended` | `broken`

A `broken` VM can only be destroyed. If a `running` VM's PID is stale, it auto-transitions to `stopped`. A `suspended` VM has its full state (RAM + devices) saved to a snapshot inside `disk.qcow2` (named `agv-suspend`); resume restarts QEMU with `-loadvm`.
