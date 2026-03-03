# Config file reference

VMs are configured with a TOML file, defaulting to `agv.toml` in the current directory.
Generate a starter file with `agv init`, or create one from scratch using the sections below.

Everything in a config file can also be expressed as CLI flags to `agv create` — each
section shows the equivalent flags.

## Overview

A config file can contain these sections:

| Section | What it does |
|---------|--------------|
| `[base]` | Base image, mixins, hardware spec, username |
| `[vm]` | Override individual resource settings |
| `[[files]]` | Files to copy from the host into the VM |
| `[[setup]]` | Commands run as root before provisioning |
| `[[provision]]` | Commands run as your user after setup |

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

All `include` fields accumulate — each mixin adds its own `files`, `setup`, and `provision`
steps before your own.

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

`setup` and `provision` steps from mixins run before your own steps, in the order the
mixins are listed.

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
