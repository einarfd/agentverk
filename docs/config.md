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

## `forwards`

Port forwards from the host into the VM, applied automatically on every `agv start`
or `agv resume`. Each forward runs as a small supervisor process around
`ssh -N -L`, so services bound to `127.0.0.1` inside the guest are reachable —
and the supervisor reconnects on its own if SSH drops temporarily.

```toml
forwards = [
  "8080",         # host:8080 → VM:8080 (tcp, default)
  "5433:5432",    # host:5433 → VM:5432
  "53/udp",       # UDP
  "9000:9000/udp",
]
```

Each entry is `HOST[:GUEST][/PROTO]`. When `GUEST` is omitted it defaults to the same
value as `HOST`; `PROTO` is `tcp` unless set to `udp`.

Runtime changes made via `agv forward` (adding or stopping forwards) are **ephemeral** —
the next start/resume resets the set back to what the config declares. To change the
persistent set without editing the config file, use `agv config set --forwards "..."`
(replaces the list wholesale).

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
3. Host environment variables

## The `.env` file

Put secrets next to `agv.toml` in a `.env` file. **Add `.env` to `.gitignore`** so
tokens are never committed.

```sh
# .env
GITHUB_TOKEN=ghp_...
```

Supported formats: `KEY=value`, `KEY="quoted value"`, `KEY='single quoted'`.
Lines starting with `#` are ignored.

## Precedence

When the same setting is specified in multiple places, the order of priority is
(highest wins):

1. **CLI flags** — `--memory`, `--cpus`, `--disk`, `--include`, `--spec`, etc.
2. **`agv.toml`** (or the file passed with `--config`)
3. **Named spec** — `spec = "..."`, default: `medium` (2G RAM, 2 vCPUs, 20G disk)
