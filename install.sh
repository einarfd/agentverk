#!/usr/bin/env sh
# Install agv — QEMU VM manager for AI coding agents.
# Usage: curl -fsSL https://raw.githubusercontent.com/einarfd/agentverk/main/install.sh | sh
# Or:    sh install.sh [--dest /usr/local/bin] [--version v0.3.0]

set -eu

REPO="einarfd/agentverk"
DEST=""
VERSION=""

# Color output when stdout is a TTY and NO_COLOR is not set. Leaves the
# variables empty otherwise, so unstyled output falls through unchanged
# for logs, CI captures, and NO_COLOR users.
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    RED=$(printf '\033[31m')
    GREEN=$(printf '\033[32m')
    YELLOW=$(printf '\033[33m')
    RESET=$(printf '\033[0m')
else
    RED=""
    GREEN=""
    YELLOW=""
    RESET=""
fi

usage() {
    cat <<'USAGE'
Usage: install.sh [--dest DIR] [--version TAG]

  --dest DIR      Install into DIR (default: /usr/local/bin when
                  writable, otherwise ~/.local/bin)
  --version TAG   Install a specific release (e.g. v0.3.0) instead of
                  the latest one
  -h, --help      Show this message
USAGE
}

# Parse flags. An unrecognized argument is an error rather than
# something to skip past: the loop used to ignore them while DEST was
# seeded from "$1", so a typo like `--dset ~/bin` installed into a
# directory named `--dset` without a word of complaint.
while [ $# -gt 0 ]; do
    case "$1" in
        --dest)
            [ $# -ge 2 ] || {
                echo "${RED}error:${RESET} --dest requires a directory" >&2
                exit 1
            }
            DEST="$2"; shift 2 ;;
        --dest=*) DEST="${1#--dest=}"; shift ;;
        --version)
            [ $# -ge 2 ] || {
                echo "${RED}error:${RESET} --version requires a release tag" >&2
                exit 1
            }
            VERSION="$2"; shift 2 ;;
        --version=*) VERSION="${1#--version=}"; shift ;;
        -h|--help) usage; exit 0 ;;
        *)
            echo "${RED}error:${RESET} unknown argument: $1" >&2
            usage >&2
            exit 1 ;;
    esac
done

# Release tags are v-prefixed. Accept either form so `--version 0.3.0`
# works as well as `--version v0.3.0` — the bare number is what people
# read off `agv --version`.
if [ -n "$VERSION" ]; then
    case "$VERSION" in
        v*) ;;
        *) VERSION="v$VERSION" ;;
    esac
fi

# Detect OS
OS=$(uname -s)
ARCH=$(uname -m)

case "$OS" in
    Darwin)
        case "$ARCH" in
            arm64|aarch64) TARGET="aarch64-apple-darwin" ;;
            *)
                echo "${RED}error:${RESET} unsupported macOS architecture: $ARCH" >&2
                echo "agv supports Apple Silicon (arm64) Macs." >&2
                exit 1
                ;;
        esac
        ;;
    Linux)
        case "$ARCH" in
            x86_64)  TARGET="x86_64-unknown-linux-musl" ;;
            aarch64) TARGET="aarch64-unknown-linux-musl" ;;
            *)
                echo "${RED}error:${RESET} unsupported Linux architecture: $ARCH" >&2
                echo "agv supports x86_64 and aarch64 Linux." >&2
                exit 1
                ;;
        esac
        ;;
    *)
        echo "${RED}error:${RESET} unsupported OS: $OS" >&2
        echo "agv supports macOS and Linux." >&2
        exit 1
        ;;
esac

# Choose install destination
if [ -z "$DEST" ]; then
    if [ -d "/usr/local/bin" ] && [ -w "/usr/local/bin" ]; then
        DEST="/usr/local/bin"
    elif [ -d "$HOME/.local/bin" ]; then
        DEST="$HOME/.local/bin"
    else
        DEST="$HOME/.local/bin"
        mkdir -p "$DEST"
    fi
fi

BIN="$DEST/agv"
TARBALL="agv-$TARGET.tar.gz"

# An unpinned install follows GitHub's `latest` redirect. Pinning
# resolves to that one release's asset path instead — without it there
# is no way to install a known version, which is what CI that
# curl-pipes this script needs to stay reproducible.
if [ -n "$VERSION" ]; then
    BASE="https://github.com/$REPO/releases/download/$VERSION"
else
    BASE="https://github.com/$REPO/releases/latest/download"
fi
URL="$BASE/$TARBALL"

echo "Detected: $OS/$ARCH ($TARGET)"
echo "Version: ${VERSION:-latest}"
echo "Installing to: $DEST"
echo ""

# Download into a temp dir, extract, then move the binaries into
# place. Tar rather than raw binary so the macOS release can ship
# both `agv` and `agv-avf-runner` (the Apple Virtualization
# supervisor) in one install step — without the latter, the
# default `avf` backend on macOS Apple Silicon would have nothing
# to spawn.
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

echo "Downloading $TARBALL..."
if command -v curl >/dev/null 2>&1; then
    curl -fsSL --progress-bar -o "$TMP/$TARBALL" "$URL"
elif command -v wget >/dev/null 2>&1; then
    wget -q --show-progress -O "$TMP/$TARBALL" "$URL"
else
    echo "${RED}error:${RESET} neither curl nor wget found. Please install one and retry." >&2
    exit 1
fi

# Verify the tarball against the release's checksums.txt. The tarball
# and the checksums come from the same origin, so this is not
# protection against a compromised release — it catches truncated
# downloads, proxy corruption, and a partially-replaced asset. Real
# tamper-evidence would need signing or build provenance.
#
# Linux ships sha256sum, macOS ships shasum. A host with neither still
# installs: a missing checksum tool is not a reason to refuse.
if command -v sha256sum >/dev/null 2>&1; then
    SHA_CHECK="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
    SHA_CHECK="shasum -a 256"
else
    SHA_CHECK=""
fi

if [ -z "$SHA_CHECK" ]; then
    echo "${YELLOW}warning:${RESET} no sha256 tool found; skipping checksum verification" >&2
else
    echo "Verifying checksum..."
    # No -S here, unlike the tarball fetch: a release without a
    # checksums.txt is handled below with our own warning, and curl's
    # raw 404 ahead of it just looks like a failed install.
    if command -v curl >/dev/null 2>&1; then
        curl -fsL -o "$TMP/checksums.txt" "$BASE/checksums.txt" || true
    else
        wget -q -O "$TMP/checksums.txt" "$BASE/checksums.txt" || true
    fi

    # Pull out this target's line. Nothing to check means the release
    # predates checksums.txt or doesn't list this asset — warn rather
    # than fail, so an older pinned version stays installable.
    EXPECTED=""
    if [ -f "$TMP/checksums.txt" ]; then
        EXPECTED=$(grep " $TARBALL\$" "$TMP/checksums.txt" || true)
    fi

    if [ -z "$EXPECTED" ]; then
        echo "${YELLOW}warning:${RESET} no published checksum for $TARBALL; skipping verification" >&2
    elif echo "$EXPECTED" | (cd "$TMP" && $SHA_CHECK -c -) >/dev/null 2>&1; then
        echo "${GREEN}Checksum OK${RESET}"
    else
        echo "${RED}error:${RESET} checksum mismatch for $TARBALL" >&2
        echo "The download is corrupt or incomplete. Retry the install; if it" >&2
        echo "keeps failing, report it at https://github.com/$REPO/issues" >&2
        exit 1
    fi
fi

echo "Extracting..."
tar -xzf "$TMP/$TARBALL" -C "$TMP"

# Move every binary the tarball ships into DEST. The macOS tarball
# adds `agv-avf-runner` alongside `agv`; the Linux tarballs contain
# just `agv`.
SRC_DIR="$TMP/agv-$TARGET"
if [ ! -d "$SRC_DIR" ]; then
    echo "${RED}error:${RESET} extracted tarball missing expected directory $SRC_DIR" >&2
    exit 1
fi
for binary in "$SRC_DIR"/*; do
    name=$(basename "$binary")
    install -m 0755 "$binary" "$DEST/$name"
    echo "${GREEN}Installed${RESET} $name to $DEST/$name"
done
echo ""

# Verify the binary runs
if ! "$BIN" --version >/dev/null 2>&1; then
    echo "${YELLOW}warning:${RESET} installed binary did not respond to --version. It may not work on this system." >&2
fi

# PATH hint if DEST not in PATH
case ":$PATH:" in
    *":$DEST:"*) ;;
    *)
        echo "Note: $DEST is not in your PATH."
        echo "Add this to your shell profile (~/.bashrc, ~/.zshrc, etc.):"
        echo ""
        echo "  export PATH=\"$DEST:\$PATH\""
        echo ""
        ;;
esac

# Run dependency check
echo "Checking dependencies..."
echo ""
"$BIN" doctor || true

echo ""
echo "${GREEN}Done!${RESET} Run 'agv --help' to get started."
