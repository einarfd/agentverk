# agv dev tasks. `just --list` to see them all.
#
# Standard cargo commands still work directly (`cargo build`, `cargo test`,
# etc.); this file just chains the common multi-step workflows so they
# don't need to be remembered. Designed to grow as more steps appear
# (Swift helper builds, release tarball assembly, etc.).

# Default recipe lists what's available.
default:
    @just --list

# Build the debug binary.
build:
    cargo build

# Build the release binary (LTO enabled in Cargo.toml's release profile).
build-release:
    cargo build --release

# Run the fast test suite (no slow boot tests, no real cloud-image downloads).
test:
    cargo test

# Run the full test suite, including the #[ignore]'d slow boot tests (downloads cloud images, boots VMs — minutes per test).
test-slow:
    cargo test -- --include-ignored --nocapture

# Lint with clippy::pedantic. Must pass with zero warnings.
clippy:
    cargo clippy --all-targets --all-features

# Run clippy + the fast test suite. Pre-commit gate.
verify: clippy test

# Run clippy + the full test suite (including slow boot tests).
verify-slow: clippy test-slow
