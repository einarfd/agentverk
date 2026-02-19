# CLAUDE.md

## Project overview

`agv` is a Rust CLI tool for creating and managing QEMU/KVM microVMs for AI coding agents. Each VM is an isolated Linux environment with SSH access, provisioned from a TOML config file.

## Build and test

```bash
cargo build          # Build debug binary
cargo clippy         # Lint — must pass with zero warnings
cargo test           # Run all tests
cargo build --release  # Release build (LTO enabled)
```

The binary is at `./target/debug/agv` (or `./target/release/agv`).

## Architecture

- **Entry point**: `src/main.rs` — tracing init, CLI parse, error display
- **Command dispatch**: `src/lib.rs` — matches CLI subcommand and calls into modules
- **CLI definition**: `src/cli.rs` — clap derive structs for all commands and flags
- **Config**: `src/config.rs` — serde structs for `agv.toml` parsing
- **Errors**: `src/error.rs` — `thiserror` enum with all error variants
- **VM lifecycle**: `src/vm/mod.rs` — orchestrates create/start/stop/destroy
- **Instance state**: `src/vm/instance.rs` — on-disk state, status reconciliation
- **QEMU**: `src/vm/qemu.rs` — process spawning and QMP protocol
- **Cloud-init**: `src/vm/cloud_init.rs` — seed image generation
- **SSH**: `src/ssh.rs` — shells out to system `ssh`/`scp`
- **Images**: `src/image.rs` — download, cache, checksum, qcow2 overlays
- **Directories**: `src/dirs.rs` — platform-specific paths (macOS/Linux)

## Conventions

- **Error handling**: `anyhow::Result` for application code, `thiserror` for library error types in `error.rs`
- **Async runtime**: Tokio — all I/O operations are async
- **Lints**: `clippy::pedantic` is enabled; all warnings must be fixed before committing
- **Unsafe**: Forbidden (`unsafe_code = "forbid"` in Cargo.toml)
- **Edition**: Rust 2024

## VM state storage

- macOS: `~/Library/Application Support/agv/`
- Linux: `~/.local/share/agv/`

Instance state lives in `instances/<name>/` with files: `disk.qcow2`, `seed.iso`, `id_ed25519`, `config.toml`, `status`, `pid`, `qmp.sock`, `error.log`.

## VM statuses

`creating` | `running` | `stopped` | `broken`

A `broken` VM can only be destroyed. If a `running` VM's PID is stale, it auto-transitions to `stopped`.
