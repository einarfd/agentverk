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

# Build the Apple Virtualization helper binary (macOS only). No-op on Linux.
# Ad-hoc signs the binary with the `com.apple.security.virtualization`
# entitlement; without it, AVF API calls fail with VZErrorDomain Code=2.
build-avf-runner:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ "$(uname -s)" != "Darwin" ]]; then
        echo "agv-avf-runner builds only on macOS — skipping"
        exit 0
    fi
    cd swift/avf-runner
    swift build -c release
    codesign --sign - --entitlements entitlements.plist --force \
        .build/release/agv-avf-runner

# Install agv from source — runs `cargo install --path .` and, on
# macOS, also builds and installs `agv-avf-runner` alongside it.
# Without the sibling runner, `agv` falls back to QEMU even where AVF
# would be the default and `--backend avf` fails with a clear error
# from `agv doctor`.
#
# Honors cargo's standard install-location lookup: `CARGO_INSTALL_ROOT`
# wins, then `CARGO_HOME`, then `$HOME/.cargo`. The runner lands at
# `<root>/bin/agv-avf-runner` so `locate_avf_runner`'s
# sibling-of-current-exe fallback picks it up automatically.
install:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo install --path .
    if [[ "$(uname -s)" != "Darwin" ]]; then
        exit 0
    fi
    install_root="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}"
    just build-avf-runner
    install -m 0755 \
        swift/avf-runner/.build/release/agv-avf-runner \
        "$install_root/bin/agv-avf-runner"
    echo "agv-avf-runner installed at $install_root/bin/agv-avf-runner"

# Run the Swift unit tests for the AVF runner's pure-logic helpers
# (LeaseLookup, …). macOS only; no-op on Linux. Uses Swift Testing
# (`import Testing`) — works on Command Line Tools alone, no full
# Xcode required, but Xcode users get the same result.
#
# Why the explicit -F / -rpath flags: Command Line Tools ships the
# Testing framework under /Library/Developer/CommandLineTools/.../
# Frameworks but doesn't put it on the default search paths the way
# Xcode does. The flags are no-ops under full Xcode (the path simply
# won't exist, swift ignores missing -F paths).
test-avf-runner:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ "$(uname -s)" != "Darwin" ]]; then
        echo "agv-avf-runner tests run only on macOS — skipping"
        exit 0
    fi
    cd swift/avf-runner
    framework_dir=/Library/Developer/CommandLineTools/Library/Developer/Frameworks
    swift test \
        -Xswiftc -F -Xswiftc "$framework_dir" \
        -Xlinker -F -Xlinker "$framework_dir" \
        -Xlinker -rpath -Xlinker "$framework_dir"

# Run the fast test suite (no slow boot tests, no real cloud-image downloads).
test:
    cargo test

# Run the full test suite, including the #[ignore]'d slow boot tests (downloads cloud images, boots VMs — minutes per test).
test-slow:
    cargo test -- --include-ignored --nocapture

# Lint with clippy::pedantic. Must pass with zero warnings.
# `-D warnings` matches the CI invocation in `.github/workflows/ci.yml`
# so `just verify` and CI agree: a passing local run guarantees the
# clippy step on either CI leg.
clippy:
    cargo clippy --all-targets --all-features -- -D warnings

# Run clippy + the fast test suite + Swift unit tests. Pre-commit gate.
verify: clippy test test-avf-runner

# Run clippy + the full test suite (including slow boot tests) + Swift unit tests.
verify-slow: clippy test-slow test-avf-runner
