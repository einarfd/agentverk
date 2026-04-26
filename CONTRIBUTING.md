# Contributing to agv

Thanks for your interest in contributing. This file captures the handful of
things worth knowing before you open a PR.

## Getting started

```sh
git clone https://github.com/einarfd/agentverk.git
cd agentverk
cargo build                         # debug binary → ./target/debug/agv
cargo test                          # fast tests (no VM boot)
./target/debug/agv doctor           # check required host dependencies
```

`AGENTS.md` has the full architecture overview and project conventions.

## Reporting bugs and requesting features

- For bugs, open a GitHub issue with the `agv --version`, your host OS/arch,
  and steps to reproduce.
- For security issues, see [`SECURITY.md`](SECURITY.md) — please don't open a
  public issue for vulnerabilities.
- For non-trivial changes (new commands, reworking existing behavior, adding
  dependencies), please open an issue first so we can agree on the direction
  before you invest time in a PR.

## Code quality bar

- `cargo clippy --all-targets -- -D warnings` must pass. `clippy::pedantic`
  is on; fix warnings rather than silencing them.
- If you genuinely need to suppress a lint, use
  `#[expect(clippy::foo, reason = "...")]`, not `#[allow(...)]`.
- `unsafe_code` is forbidden at the crate level.
- `anyhow::Result` for application code, `thiserror` enum in `src/error.rs`
  for library error types.
- All I/O is async on Tokio.

## Tests

The test suite has three categories (see `AGENTS.md` for the full policy):

1. **Always-on, no external tools** — pure logic. Run on every `cargo test`.
2. **Runtime-skip integration** — spawns `qemu-img`, brief QEMU processes,
   etc. Runs on every `cargo test` but gracefully skips when the tool is
   missing.
3. **Slow boot tests** — download a real cloud image and boot a VM. Marked
   `#[ignore]`, run with `cargo test -- --include-ignored --nocapture`.

**Please run the slow boot suite before submitting a PR that touches the VM
lifecycle, provisioning, SSH integration, or templates.** They take a few
minutes (the first run downloads ~330 MB; later runs use the image cache)
and catch real regressions the fast suite cannot. You need QEMU installed;
`agv doctor` will tell you if you're missing anything.

## Commit messages

Look at recent `git log` for the style: short imperative subject line, no
scope prefix, no trailing period. A short body explaining *why* (not *what*)
is welcome when the subject alone doesn't convey it. Keep unrelated changes
in separate commits.

## PR checklist

Before marking a PR ready:

- [ ] `cargo clippy --all-targets -- -D warnings` is clean.
- [ ] `cargo test` passes.
- [ ] Slow boot suite (`cargo test -- --include-ignored --nocapture`) passes
      if your change touches VM lifecycle, provisioning, SSH, or templates.
- [ ] `docs/` and `examples/` are updated if behavior changed.
- [ ] `CHANGELOG.md` has an entry under `## [Unreleased]` for user-visible
      changes (new commands, new config fields, behavior changes, bug fixes).
- [ ] CI is green.

## Cutting a release

Release flow (one-person project; `cargo publish` runs from a trusted
local machine, not CI, on purpose):

1. Bump `version` in `Cargo.toml` to the new value.
2. Close out the `## [Unreleased]` section in `CHANGELOG.md`: add a new
   `## [X.Y.Z] - YYYY-MM-DD` heading just below it, leaving `[Unreleased]`
   as an empty placeholder for the next cycle. Update the compare links
   at the bottom of the file.
3. `cargo build` to refresh `Cargo.lock` to the new version.
4. Commit as `Release X.Y.Z`.
5. Run the release sanity check:

   ```sh
   ./scripts/release-check.sh
   ```

   It verifies the working tree is clean, you're on `main`, all version
   metadata agrees, the tag isn't already present, lint/tests/dry-run
   pass — exactly the class of mistake (tag pushed before bump) that
   `release-check.sh` exists to catch.

6. If the script's green, follow the suggested next steps it prints —
   `git tag -a vX.Y.Z`, `git push origin main`, `git push origin vX.Y.Z`,
   `cargo publish`. The tag push triggers the GitHub Release workflow
   (cross-platform binaries + `install.sh` redirect target).
