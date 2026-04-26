#!/usr/bin/env bash
# Pre-release sanity check.
#
# Run AFTER committing the "Release X.Y.Z" commit (with bumped Cargo.toml,
# refreshed Cargo.lock, and a closed-out CHANGELOG section) but BEFORE
# `git tag` / `git push` / `cargo publish`. Asserts that the working tree,
# version metadata, branch, and tests all agree before a release goes out.
#
# Designed to catch the class of mistake where a tag gets pushed before
# the version bump (then `cargo publish --dry-run` reports the wrong
# version while the GitHub Release workflow has already built artifacts
# from the unbumped Cargo.toml).
#
# Usage:
#   ./scripts/release-check.sh
#
# Exit codes:
#   0   all checks passed
#   1   a check failed

set -euo pipefail

if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    RED=$'\033[31m'; GREEN=$'\033[32m'; YELLOW=$'\033[33m'; RESET=$'\033[0m'
else
    RED=""; GREEN=""; YELLOW=""; RESET=""
fi

ok()   { echo "${GREEN}✓${RESET} $1"; }
warn() { echo "${YELLOW}→${RESET} $1"; }
fail() { echo "${RED}✗ $1${RESET}" >&2; exit 1; }

cd "$(git rev-parse --show-toplevel)"

# 1. Clean tree.
if ! git diff --quiet HEAD; then
    fail "working tree is dirty (uncommitted changes) — commit or stash before releasing"
fi
if [ -n "$(git status --porcelain)" ]; then
    fail "untracked or unstaged files present — clean up before releasing"
fi
ok "working tree is clean"

# 2. On main.
BRANCH=$(git rev-parse --abbrev-ref HEAD)
if [ "$BRANCH" != "main" ]; then
    fail "on branch '$BRANCH', expected 'main'"
fi
ok "on main"

# 3. Version from Cargo.toml.
VERSION=$(grep -m1 '^version' Cargo.toml | sed 's/^version *= *"\(.*\)"/\1/')
if [ -z "$VERSION" ]; then
    fail "could not read version from Cargo.toml"
fi
ok "Cargo.toml version: $VERSION"

# 4. Cargo.lock matches.
LOCK_VERSION=$(awk '/^name = "agv"$/{getline; print; exit}' Cargo.lock \
    | sed 's/^version *= *"\(.*\)"/\1/')
if [ "$LOCK_VERSION" != "$VERSION" ]; then
    fail "Cargo.lock has agv at $LOCK_VERSION but Cargo.toml says $VERSION — run \`cargo build\` to refresh"
fi
ok "Cargo.lock matches Cargo.toml"

# 5. Last commit subject is "Release $VERSION".
LAST_SUBJECT=$(git log -1 --pretty=%s)
if [ "$LAST_SUBJECT" != "Release $VERSION" ]; then
    fail "last commit subject is \"$LAST_SUBJECT\", expected \"Release $VERSION\""
fi
ok "last commit is the release commit"

# 6. CHANGELOG has a dated entry for this version.
if ! grep -qE "^## \[$VERSION\] - [0-9]{4}-[0-9]{2}-[0-9]{2}" CHANGELOG.md; then
    fail "CHANGELOG.md has no \"## [$VERSION] - YYYY-MM-DD\" heading"
fi
ok "CHANGELOG.md has dated entry for $VERSION"

# 7. CHANGELOG compare links updated.
if ! grep -q "^\[$VERSION\]: https://github" CHANGELOG.md; then
    fail "CHANGELOG.md missing compare link for $VERSION"
fi
if ! grep -q "^\[Unreleased\]: https://github\.com/.*/compare/v$VERSION\.\.\.HEAD" CHANGELOG.md; then
    fail "CHANGELOG.md \"[Unreleased]: …compare/vX…HEAD\" link doesn't point at v$VERSION"
fi
ok "CHANGELOG.md compare links updated"

# 8. Tag must not exist yet (locally or on origin).
if git rev-parse "v$VERSION" >/dev/null 2>&1; then
    fail "tag v$VERSION already exists locally — \`git tag -d v$VERSION\` to remove and re-run"
fi
if git ls-remote --tags origin "v$VERSION" 2>/dev/null | grep -q .; then
    fail "tag v$VERSION already exists on origin — clean up before continuing"
fi
ok "tag v$VERSION does not exist yet"

# 9. Lint, tests, and crate dry-run.
warn "cargo clippy --all-targets -- -D warnings"
cargo clippy --all-targets --quiet -- -D warnings
ok "clippy clean"

warn "cargo test"
cargo test --quiet >/dev/null
ok "tests pass"

warn "cargo publish --dry-run"
# Capture full output so we can show it on failure; suppress when it's clean.
PUBLISH_LOG=$(mktemp)
trap 'rm -f "$PUBLISH_LOG"' EXIT
if ! cargo publish --dry-run >"$PUBLISH_LOG" 2>&1; then
    cat "$PUBLISH_LOG"
    fail "cargo publish dry-run failed"
fi
ok "cargo publish dry-run succeeded"

echo
echo "${GREEN}All checks passed.${RESET}  Suggested next steps:"
echo "  git tag -a v$VERSION -m \"Release $VERSION\""
echo "  git push origin main"
echo "  git push origin v$VERSION"
echo "  cargo publish"
