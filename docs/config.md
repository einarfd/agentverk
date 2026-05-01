# Config file reference

VMs can be configured with a TOML file passed to `agv create --config <path>`.
Generate a starter file with `agv init -o <path>`, or create one from scratch
using the sections below.

Everything in a config file can also be expressed as CLI flags to `agv create` — each
section shows the equivalent flags.

> **Note:** `agv create` does not look for `agv.toml` in the current directory —
> the `--config` flag is required if you want to use a config file. Without it,
> `agv create` falls back to the built-in `ubuntu-24.04` image.

## Overview

A config file can contain these sections:

| Section | What it does |
|---------|--------------|
| `[base]` | Base image, mixins, hardware spec, username |
| `[vm]` | Override individual resource settings |
| `[[files]]` | Files to copy from the host into the VM |
| `[[setup]]` | Commands run as root before provisioning |
| `[[provision]]` | Commands run as your user after setup |
| `forwards` | Host→guest port forwards applied on every start/resume |

`[[files]]`, `[[setup]]`, and `[[provision]]` can each appear multiple times and run in
the order listed. Sections from mixins (`include`) run before your own.

## `[base]`

```toml
[base]
from    = "ubuntu-24.04"           # base image  (run `agv images` to list all)
include = ["devtools", "claude"]   # mixins       (run `agv images` to list all)
spec    = "large"                  # hardware preset — optional, default: "medium"
user    = "agent"                  # VM username — optional, default: "agent"
```

CLI equivalents:

| TOML field | CLI flag |
|------------|----------|
| `from = "ubuntu-24.04"` | `--image ubuntu-24.04` |
| `include = ["devtools", "claude"]` | `--include devtools --include claude` |
| `spec = "large"` | `--spec large` |

The `user` field has no CLI equivalent — it can only be set in the config file.

**Shorthands**: `--image ubuntu`, `--image debian`, and `--image fedora` are
accepted as aliases for the current canonical versions (`ubuntu-24.04`,
`debian-12`, `fedora-43`). Aliases resolve at CLI-parse time; the VM's
saved config records the concrete URL, so they never introduce ambiguity
after creation. For stability in scripts, prefer the canonical names —
aliases may move when a newer release ships.

All `include` fields accumulate — each mixin adds its own `files`, `setup`, and `provision`
steps before your own.

### `os_family`

Root images (the ones with `aarch64` / `x86_64` URL sections) must declare an
`os_family`. It tells the resolver which `[os_families.<name>]` mixin sections
to apply, and lets mixins declare which package-manager dialect they assume.

```toml
[base]
os_family = "debian"   # or "fedora", "alpine", etc.

[base.aarch64]
url      = "..."
checksum = "..."
```

Child images (those with `from = "..."`) inherit `os_family` from their
parent automatically — you only need to set it on the root image. Currently
shipped os_families: `debian` (Ubuntu 24.04, Debian 12) and `fedora`
(Fedora 43). Alpine support is planned but requires x86_64 UEFI support
in the QEMU layer first.

## `[vm]`

Override individual resource settings on top of the named `spec`. This section is
optional — omit it entirely to use the spec values unchanged. Any field omitted here
falls back to the spec value.

```toml
[vm]
memory = "8G"   # e.g. 512M, 2G, 16G
cpus   = 4      # number of virtual CPUs
disk   = "40G"  # e.g. 10G, 40G, 100G
```

CLI equivalents: `--memory 8G`, `--cpus 4`, `--disk 40G`

Disk can only be grown after creation, not shrunk. After resizing with `agv config set`,
run `growpart` and `resize2fs` inside the VM to use the extra space.

### `idle_suspend_minutes`

Auto-suspend the VM after this many minutes of confirmed idleness. Disabled by default
(`0` or unset). Idle is the AND of "no interactive SSH session" (port-forward supervisors
run `ssh -N` and don't allocate a PTY, so they don't count) and "guest 5-min load average
below `idle_load_threshold`".

```toml
[vm]
idle_suspend_minutes = 30   # save state and exit QEMU after 30 idle minutes
idle_load_threshold  = 0.2  # optional; default 0.2
```

A long-running tmux agent will keep the VM up via the load signal even if no SSH
session is currently attached. Resume with `agv resume <name>`.

## `[[files]]`

Copy files or directories from the host into the VM before any provisioning runs.

```toml
[[files]]
source = "{{HOME}}/.gitconfig"          # host path — use {{HOME}}, not ~/
dest   = "/home/{{AGV_USER}}/.gitconfig"

[[files]]
source = "./scripts"
dest   = "/home/{{AGV_USER}}/scripts"
```

CLI equivalent: `--file {{HOME}}/.gitconfig:/home/{{AGV_USER}}/.gitconfig` (repeatable)

Both `source` and `dest` support template variable expansion (see below). Note that
`~/` is **not** expanded — use `{{HOME}}` for host paths and `/home/{{AGV_USER}}`
for paths inside the VM. `{{AGV_USER}}` is set to the VM's username (default: `agent`).

### `optional = true`

By default, a missing source path on the host is a hard error. Set
`optional = true` to silently skip the copy when the source isn't there.
Useful for opportunistically pulling in files that may or may not exist
in the user's home directory:

```toml
[[files]]
source   = "{{HOME}}/.ssh/id_ed25519"
dest     = "/home/{{AGV_USER}}/.ssh/id_ed25519"
optional = true
```

Pairs naturally with `{{VAR:-}}` template defaults (see below): an unset
env var resolves to an empty path which then doesn't exist, and the
optional flag turns the resulting "no such file" into a no-op.

> **Security:** Avoid copying your primary SSH key here. If the agent runs malicious
> code or the VM is compromised, the key is exposed. See [`docs/repo-access.md`](repo-access.md)
> for safer alternatives.

## `[[setup]]`

Commands run as **root** inside the VM, before provisioning. Use for system-level work:
installing packages, configuring services, creating directories with specific ownership.

```toml
[[setup]]
run = "apt-get install -y ripgrep fd-find"

[[setup]]
run = """
useradd -m extrauser
echo 'extrauser ALL=(ALL) NOPASSWD:ALL' >> /etc/sudoers
"""

[[setup]]
script = "./configure-system.sh"   # local script, copied in and executed as root
```

CLI equivalents: `--setup "SCRIPT"`, `--setup-script ./path`

`run` is an inline shell script; multiline strings (using `"""`) work fine.
`script` is a path to a local file that is copied into the VM and executed.

`run` can also take an array of strings to save repeated `[[setup]]` headers —
each entry becomes its own step, in order:

```toml
[[setup]]
run = [
  "apt-get update",
  "apt-get install -y ripgrep fd-find",
  "systemctl disable --now unattended-upgrades",
]
```

This is equivalent to writing three `[[setup]]` blocks — retry granularity and
interactive-mode prompts work per entry. A block uses exactly one of `run` or
`script`.

## `[[provision]]`

Commands run as **your user** (default: `agent`) after setup completes and SSH is
available. Use for user-level setup: cloning repositories, configuring tools, installing
dotfiles.

```toml
[[provision]]
run = "git clone git@github.com:org/repo.git ~/repo"

[[provision]]
run = """
cd ~/repo
./bootstrap.sh
"""

[[provision]]
script = "./user-setup.sh"   # local script, copied in and executed as your user
```

CLI equivalents: `--provision "SCRIPT"`, `--provision-script ./path`

As with `[[setup]]`, `run` can be an array of strings — each entry becomes its
own step:

```toml
[[provision]]
run = [
  "gh auth login --with-token <<< '{{GITHUB_TOKEN}}'",
  "gh auth setup-git",
  "gh repo clone org/myrepo ~/myrepo",
]
```

`setup` and `provision` steps from mixins run before your own steps, in the order the
mixins are listed.

## `supports` and `[os_families.<name>]` (mixin authors)

A mixin can target one or more OS families. The base image declares its
`os_family`; each mixin must be reachable from there.

**Distro-agnostic mixin** (works on every family):

```toml
# Just top-level steps. No `supports` and no `[os_families.*]`.
[[provision]]
run = "curl -LsSf https://astral.sh/uv/install.sh | sh"
```

**Same script, only on listed families** — use `supports`. Catches things
like precompiled binaries that won't run on Alpine (musl) even though the
install script looks distro-agnostic:

```toml
supports = ["debian", "fedora"]   # alpine users get a clear error

[[provision]]
run = "curl -fsSL https://example.com/install.sh | bash"
```

**Different script per family** — use `[os_families.<name>]`:

```toml
[os_families.debian]
[[os_families.debian.setup]]
run = "apt-get update && apt-get install -y git curl build-essential"

[os_families.fedora]
[[os_families.fedora.setup]]
run = "dnf install -y git curl gcc make"

[os_families.alpine]
[[os_families.alpine.setup]]
run = "apk add --no-cache git curl build-base"
```

The resolver picks the section matching the base's `os_family`. The implicit
support list is exactly the family keys present.

**Shared setup + per-family install** — combine top-level steps with
`[os_families.*]` sections. Top-level steps run for every supported family;
the matching family section's steps are appended afterward:

```toml
supports = ["debian", "fedora"]

[[provision]]
run = "mkdir -p ~/.foo"            # runs on both

[os_families.debian]
[[os_families.debian.setup]]
run = "apt-get install -y libfoo"

[os_families.fedora]
[[os_families.fedora.setup]]
run = "dnf install -y libfoo"
```

**Rules**:

- If a `[os_families.<name>]` section has steps, that family must also appear
  in `supports` (when `supports` is set). Otherwise the mixin would
  silently ship steps for an unsupported family.
- A mixin with `[os_families.*]` sections but no `supports` implicitly
  supports exactly those family keys.
- A mixin with neither is treated as distro-agnostic and runs on every
  family.

When the resolved family isn't supported, the resolver fails fast with:

```
mixin 'devtools' does not support os_family 'alpine'
  base image os_family: alpine
  mixin supports: debian, fedora
```

## `notes` (mixin authors)

Short lines a mixin can ship describing what it provides or any
non-obvious state it establishes. They end up in `~/.agv/system.md`
(written inside each VM at first boot) so agents running in the VM
can see the lay of the land in a single read.

```toml
# Distro-agnostic note:
notes = [
  "Installs `uv`.",
]

# Family-specific note:
[os_families.debian]
notes = [
  "Uses the Debian repo's version of `uv` rather than the install script.",
]
```

Keep notes terse — one short line per entry. Lead with what the mixin
installs; add state-changes (service enabled, default-shell swap,
group membership) only when they're not obvious from the mixin name.
A mixin with no notes still appears in the file by name.

The field is optional everywhere; omit it when there's nothing
useful to say.

`notes = [...]` also works at the **top level of your own `agv.toml`**.
Top-level notes describe *this VM* (e.g. "this VM is for the foo
project") rather than what a mixin contributed, and the renderer
surfaces them in their own `## This VM` section above the mixin list.

## `manual_steps` (mixin authors)

Imperative instructions that the **human invoker** needs to follow
after agv finishes — things agv can't automate (browser-based auth
flows, interactive token entry, anything requiring a person at the
keyboard). Printed to the host terminal at the end of the first
successful provision and surfaced again by `agv inspect <vm>` for
later re-reading.

```toml
manual_steps = [
  "Run `claude /login` inside the VM to authenticate Claude Code.",
]

# Family-specific manual steps work the same way as family-specific notes.
[os_families.debian]
manual_steps = [
  "Run `sudo dpkg-reconfigure tzdata` if the timezone matters.",
]
```

Manual steps are **never written to the VM** — agents inside don't see
them, and shouldn't (they describe tasks only a human can complete).

Use sparingly. Anything agv can do in a `[[setup]]` or `[[provision]]`
step belongs there, not in `manual_steps`. Reach for this field only
when there's a real human-in-the-loop requirement.

`manual_steps = [...]` also works at the top level of your own
`agv.toml`, for VM-specific instructions that aren't tied to a mixin
(e.g. "Configure VPN before starting work.").

## `forwards`

Port forwards from the host into the VM, applied automatically on every `agv start`
or `agv resume`. Each forward runs as a small supervisor process around
`ssh -N -L`, so services bound to `127.0.0.1` inside the guest are reachable —
and the supervisor reconnects on its own if SSH drops temporarily.

```toml
forwards = [
  "8080",         # host:8080 → VM:8080
  "5433:5432",    # host:5433 → VM:5432
  "9000:9000",    # host:9000 → VM:9000 (same port, explicit)
]
```

Each entry is `HOST[:GUEST]`. When `GUEST` is omitted it defaults to the same
value as `HOST`. TCP is implicit — the underlying `ssh -L` tunnel is TCP-only.

Runtime changes made via `agv forward` (adding or stopping forwards) are **ephemeral** —
the next start/resume resets the set back to what the config declares. To change the
persistent set without editing the config file, use `agv config set --forwards "..."`
(replaces the list wholesale).

## Desktop / GUI access

The `gui-xfce` mixin installs XFCE + TigerVNC + noVNC inside the VM,
all bound to `127.0.0.1`. It declares `[auto_forwards.gui] guest_port
= 6080`, so agv allocates a free host port and spawns an SSH-tunnel
supervisor at VM start. `agv gui <name>` reads the allocated port and
opens `http://127.0.0.1:<port>/vnc.html?autoconnect=1&resize=scale`
in the system default browser — you land straight in the XFCE desktop.

No native remote-desktop client needed; the browser handles rendering,
keyboard, clipboard, full-screen. Works identically on macOS, Linux,
and Windows hosts.

```toml
[base]
from    = "debian-12"
include = ["devtools", "gui-xfce"]
spec    = "large"    # XFCE + browser benefits from the bigger preset
```

Fedora is equally well-supported; Ubuntu works too but see the snap caveat
below before relying on `firefox` / `chromium` from the default repos. If
you just want headless development against the VM, prefer
[`docs/remote-ide.md`](remote-ide.md) over the browser desktop.

```sh
agv create --config agv.toml --start myvm
agv gui myvm
```

**Auth model**: the VNC server runs with `-SecurityTypes None` and
binds `127.0.0.1` only. The only way to reach it is through the SSH
tunnel, which is gated by the VM's unique ed25519 key. So no password
is ever embedded in a URL, saved to browser history, or stored in
localStorage — the SSH tunnel is the auth boundary (same reasoning
we already use for the `forwards` mechanism).

**Browsers on Ubuntu — avoid snaps**: Ubuntu 24.04 packages Firefox
(and Chromium) as snaps. Snap confinement expects the launching
process to sit in a cgroup hierarchy rooted at a login manager
(gdm/lightdm/sddm), and our XFCE session is started directly from a
systemd-user service. Snaps detect that and refuse to launch with
an error like `... is not a snap cgroup for tag snap.firefox.firefox`.
Install browsers via Mozilla's apt repo (Firefox .deb) or via
Flatpak (`flatpak install flathub org.mozilla.firefox`) instead.
Debian (where Firefox/Chromium are plain .debs) and Fedora are
unaffected.

## `[auto_forwards.<name>]` (mixin authors)

Named, auto-allocated port forwards — agv picks a free host port at VM
start and writes it to `<instance>/<name>_port` for other commands or
scripts to read. Unlike `forwards = [...]` (which takes explicit
`HOST[:GUEST][/PROTO]` strings), `auto_forwards` let a mixin declare
"I need a tunnel to guest port X under a stable name" without having to
pick a host port — so multiple VMs using the same mixin never collide.

This mirrors the pattern SSH already uses internally: the SSH port is
auto-allocated at VM start, written to `<instance>/ssh_port`, and stays
stable for the VM's lifetime. `auto_forwards` extends that mechanism to
arbitrary protocols declared by mixins.

```toml
# Inside a mixin — e.g. a hypothetical `gui-xfce` that exposes RDP.
[auto_forwards.rdp]
guest_port = 3389
```

TCP is implicit — the underlying tunnel is `ssh -L`, which is TCP-only.

**Discovery**:

- `agv inspect <vm>` shows each auto-forward's host port on a running VM.
- `agv forward <vm> --list` lists them alongside other active forwards
  (Origin: `auto`).
- The port is also on disk at `<instance>/<name>_port` for scripts.

**Rules**:

- Names must match `[a-z][a-z0-9_]*` — they become filenames.
- A name can only be declared once across the whole inheritance +
  include chain; duplicates fail at resolve time rather than fighting
  over the port-file path.

## Template variables

Config values support `{{VAR}}` substitution. This is the main way to pass secrets or
per-user values without hardcoding them in the config file.

```toml
[[provision]]
run = "gh auth login --with-token <<< '{{GITHUB_TOKEN}}'"

[[files]]
source = "{{HOME}}/.ssh/id_ed25519"
dest   = "/home/{{AGV_USER}}/.ssh/id_ed25519"
```

Syntax:

| Expression | Behaviour |
|------------|-----------|
| `{{VAR}}` | Required — `agv create` fails if `VAR` is not set |
| `{{VAR:-default}}` | Uses `default` if `VAR` is not set |

Built-in variables:

| Variable | Value |
|----------|-------|
| `{{AGV_USER}}` | The VM's username (same as `user` in `[base]`, default: `agent`) |

User-defined variables are resolved in this order (last wins):

1. `.env` file next to `agv.toml`
2. `.env` in the current working directory
3. The file passed via `agv create --env-file <path>`, if any
4. Host environment variables

## The `.env` file

Put secrets next to `agv.toml` in a `.env` file. **Add `.env` to `.gitignore`** so
tokens are never committed.

```sh
# .env
GITHUB_TOKEN=ghp_...
```

Supported formats: `KEY=value`, `KEY="quoted value"`, `KEY='single quoted'`.
Lines starting with `#` are ignored.

### `--env-file`

Pass `agv create --env-file /path/to/file.env <name>` to point at a `.env`
outside the implicit lookup paths — e.g. when secrets live in a shared
team location, or when you want to run with different env files per VM
without changing the working directory. Layered on top of the implicit
`.env` lookups; host environment variables still win over all of them.

Unlike the implicit `.env` lookups (which are silently skipped when the
file isn't there), `--env-file` errors out if the path doesn't exist —
you asked for it specifically.

> Anything that gets template-expanded at create time is baked into the
> saved instance config (`<data_dir>/instances/<name>/config.toml`), so
> a secret in `--env-file` lands on disk inside that file. `agv destroy`
> removes the instance dir.

## Precedence

When the same setting is specified in multiple places, the order of priority is
(highest wins):

1. **CLI flags** — `--memory`, `--cpus`, `--disk`, `--include`, `--spec`, etc.
2. **`agv.toml`** (or the file passed with `--config`)
3. **Named spec** — `spec = "..."`, default: `medium` (2G RAM, 2 vCPUs, 20G disk)
