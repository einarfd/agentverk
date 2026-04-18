# Security policy

## Reporting a vulnerability

Please report security issues privately via GitHub Security Advisories:

**<https://github.com/einarfd/agentverk/security/advisories/new>**

Include:

- The version of `agv` (`agv --version`) and your host OS/arch.
- A clear description and, if possible, a minimal reproduction.
- The impact you believe the issue has.

Please do **not** open public issues or pull requests for security bugs.

I'll acknowledge reports within 14 days and aim to ship a fix in the next
patch release. If you don't hear back within two weeks, feel free to send a
nudge through the same advisory thread.

## Supported versions

Only the latest released version receives security fixes.

## Scope

`agv` is a local developer tool. The threat model focuses on what a malicious
**config file, cloud image, or template** could do to the host, not on
arbitrary code that a user voluntarily runs inside their own VM.

**In scope:**

- Host-side command or argument injection into `ssh`, `scp`, `qemu-system-*`,
  `mkisofs`/`hdiutil`, or any other subprocess `agv` spawns.
- Path traversal or unintended writes outside `~/.local/share/agv/` (or
  `$AGV_DATA_DIR`), including through `[[files]]` copy, template handling, or
  image cache extraction.
- Tampering with `~/.ssh/config` or the managed `<data_dir>/ssh_config` beyond
  what's documented.
- Bypassing SHA-256 verification on downloaded cloud images.
- Privilege escalation on the host via the port-forward supervisor or any
  other long-running `agv` child process.
- Leakage of SSH private keys, `.env` contents, or other secrets into logs,
  process listings, or files with overly broad permissions.

**Out of scope:**

- Code a user runs inside their own VM (including `[[setup]]`/`[[provision]]`
  steps they author or paste in).
- Vulnerabilities in upstream components (QEMU, OpenSSH, cloud-init, Ubuntu
  cloud images, etc.) — please report those to their respective projects.
- Denial of service achieved only by the local user against their own
  machine (e.g. filling the disk by creating many VMs).
- Network-level attacks against guest services that the user has explicitly
  exposed via `agv forward`.

## Security-relevant behaviors

A few things worth knowing if you're auditing or integrating `agv`:

- **SSH config integration** is opt-in and scoped. `agv doctor --setup-ssh`
  adds a single `Include` line to `~/.ssh/config` pointing at a file `agv`
  manages under its data directory. No other edits are made to your SSH
  config.
- **Per-VM SSH keys.** Each VM gets its own freshly generated ED25519 keypair
  under `~/.local/share/agv/instances/<name>/`. Keys are not reused across
  VMs and are deleted on `agv destroy`.
- **Image downloads** are fetched over HTTPS and verified against a SHA-256
  checksum before use. Tampered or truncated downloads are rejected.
- **Port forwards** bind on `127.0.0.1` only. They are not reachable from
  other hosts on your network unless you explicitly configure something
  further up the stack.
- **Template expansion.** `{{VAR}}` values in config are expanded from the
  process environment and, optionally, a `.env` file next to the config.
  Treat `.env` as a secret. Tokens passed into `[[provision]]` commands are
  substituted into shell strings — be mindful of quoting, and prefer passing
  secrets via stdin (e.g. `gh auth login --with-token <<< '{{TOKEN}}'`)
  rather than as process arguments.
