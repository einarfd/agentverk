# CLAUDE.md

## Project overview

`agv` is a Rust CLI tool for creating and managing QEMU/KVM microVMs for AI agents. Each VM is an isolated Linux environment with SSH access, provisioned from a TOML config file.

## Build and test

```bash
cargo build          # Build debug binary
cargo clippy         # Lint — must pass with zero warnings
cargo test           # Run fast tests only (no QEMU needed)
cargo build --release  # Release build (LTO enabled)
```

The binary is at `./target/debug/agv` (or `./target/release/agv`).

### Test layers

**Fast tests** (`cargo test`) — always pass, no external tools needed:
- `src/` unit tests — config parsing, template expansion, SSH arg splitting, etc.
- `tests/cli_test.rs` — binary-level tests for arg parsing, help output, error messages

**VM integration tests** (`cargo test -- --include-ignored --nocapture`) — boot real VMs, require QEMU + network:
- `tests/create_test.rs` — full create/start/provision lifecycle including file injection via SCP
- `tests/qemu_test.rs` — low-level QEMU process management and QMP protocol

VM tests are `#[ignore]` by default so `cargo test` always passes in CI without QEMU. Use `--include-ignored` to run them locally. They create VMs with names prefixed `_test-` and clean up after themselves.

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
