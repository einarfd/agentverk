---
name: agv
description: Create and manage QEMU/KVM microVMs for sandboxed Linux development on macOS or Linux. Use when the task wants real isolation from the host — running untrusted code, testing risky shell commands, hosting another AI agent (Claude Code / OpenAI Codex / Google Gemini) inside the sandbox, cloning a private repo into a fresh workspace, or handing the user a disposable dev environment. Don't use for single shell commands or read-only file inspection where isolation isn't needed.
---

# agv: sandboxed Linux VMs

`agv` creates and manages QEMU/KVM microVMs on the user's host. Each VM is
a real Linux machine — SSH, Docker, and systemd all work normally inside
it. Boot takes ~30s on first create; subsequent starts and `suspend`/`resume`
are much faster.

## When to reach for agv

Reach for it when the task wants real isolation:

- Run untrusted or AI-generated code that could damage the host filesystem
- Test a destructive operation in a recoverable environment
- Host another AI agent (Claude Code, Codex, Gemini, OpenClaw)
- Clone a private repo into a workspace that can be destroyed cleanly
- Hand the user a disposable dev environment for a one-off task
- Need a Linux GUI for browser-based testing or screenshots
  (the `gui-xfce` mixin gives a noVNC desktop in the host browser)

Don't reach for it when:

- The task is a single shell command that doesn't risk the host
- You only need to read files (no execution)
- The user already has the right environment locally
- Boot latency would dominate the task

## Pre-flight

Before creating any VM:

1. **List what already exists.** Stale VMs from earlier sessions may be
   eating memory.

   ```bash
   agv ls
   ```

2. **Check that the host has capacity.** Use `agv resources` (or
   `agv resources --json` if you want to parse it):

   ```bash
   agv resources
   # Host:
   #   RAM         12.4G used of 48.0G total
   #   CPUs        14
   #   Data dir    538.3G free
   #
   # Allocated to agv VMs:
   #   Running     8.0G RAM · 4 vCPUs · 1 VM(s)
   #   Total       8.0G RAM · 4 vCPUs · 40.0G disk · 1 VM(s)
   ```

   `agv create --start` also runs a built-in capacity check: it refuses
   to boot if the new VM's memory plus already-running VM memory would
   exceed 90% of host total RAM. The error message tells you exactly
   what's happening; `--force` overrides it. Don't reach for `--force`
   reflexively — usually the right move is to stop or destroy a VM
   first. A `medium` spec is 2G/2 vCPUs, `large` is 8G/4, `xlarge` is
   16G/8.

## The 5 commands you'll use 95% of the time

```bash
agv create --start <name>         # build + boot + provision (blocks ~30–90s)
agv ssh <name> -- <command>       # run a command non-interactively
agv ssh <name>                    # interactive shell
agv cp <name> <src> <dst>         # copy files; use ":path" for VM-side paths
agv destroy --force <name>        # remove the VM and its disk
```

State discovery:

```bash
agv ls                            # list all VMs and their status
agv ls --json                     # machine-readable; prefer this for parsing
agv inspect <name>                # detailed status, mixins, manual setup steps
agv resources                     # host capacity vs. agv allocation
agv resources --json              # same, machine-readable
```

`agv inspect <name>` is also where the user's *manual setup steps* show up
— things they need to do that agv can't automate (e.g. authenticating
Claude Code if `ANTHROPIC_API_KEY` wasn't set before create).

## Naming convention

Pick a name that's clearly yours:

```
agv-<task>-<short-id>
```

For example: `agv-test-x7g2`, `agv-claude-trial-9k1m`. Avoid generic names
(`test`, `vm`, `sandbox`) — they collide with whatever else is on the host.

**Always destroy VMs when you're done with them.** Don't leave orphans for
the user to clean up. If you're handing the VM over to the user (e.g.
"here's your dev environment"), tell them the name explicitly and that
they own its lifecycle.

## Recipes

### Disposable shell sandbox

For trying a script that might be destructive:

```bash
NAME="agv-shell-$(date +%s)"
agv create --image debian --include devtools --start "$NAME"
agv ssh "$NAME" -- 'set -eux; /tmp/risky-script.sh'
agv destroy --force "$NAME"
```

### Resume across crashes / retries

If your session might be interrupted and re-run, use `--if-not-exists`
so the second invocation doesn't error on the existing VM. With
`--json`, you also get the state object back, so you can branch on
whether the VM was newly created or already there:

```bash
STATE=$(agv create --if-not-exists --start --json --include devtools agv-session-x)
echo "$STATE" | jq -r '"Created fresh? \(.created)  status: \(.status)  ssh: 127.0.0.1:\(.ssh_port)"'
# Created fresh? true   status: running   ssh: 127.0.0.1:50121
```

`--if-not-exists` only affects the `create` decision — it does not
auto-start an existing stopped VM. If you want both "ensure it
exists" and "ensure it's running", chain with `agv start`:

```bash
agv create --if-not-exists --include devtools agv-session-x
agv start agv-session-x   # no-op if already running
```

**Caveat — agv does not compare configs.** `--if-not-exists` doesn't
verify that the existing VM was created with the same `--include`,
`--memory`, etc. you're passing now. If you need to be sure the VM
is shaped right, parse the JSON output and check
`mixins_applied` / `memory` / `cpus` / `disk` against what you asked
for. If they don't match, `agv destroy` and recreate.

Notes:
- `--image debian` is shorthand for the current canonical Debian
  (`debian-12`); same shorthand works for `ubuntu`, `fedora`.
- The `devtools` mixin installs git, curl, build tools, ripgrep, jq, fd,
  tree, shellcheck, sqlite3, tmux, fzf, direnv. It's almost always
  worth including.

### Sandbox with Claude Code installed

```bash
# Set the API key on the host BEFORE create — it's baked in at provision time.
export ANTHROPIC_API_KEY=sk-...

NAME="agv-claude-$(date +%s)"
agv create --include devtools --include claude --spec large --start "$NAME"
agv ssh "$NAME"
# inside the VM:
claude
```

Equivalent for the other bundled agents:

| Mixin | Auth env var | Extra mixins | Min spec |
|---|---|---|---|
| `claude` | `ANTHROPIC_API_KEY` | — | `large` (4G+ RAM required by Claude Code) |
| `codex` | `OPENAI_API_KEY` | — | `medium` |
| `gemini` | `GEMINI_API_KEY` | needs `nodejs` | `medium` |
| `openclaw` | (no env-var support) | needs `nodejs` | `medium` |

### Clone a private GitHub repo

```bash
export GH_TOKEN=ghp_...                    # or GITHUB_TOKEN; GH_TOKEN wins

NAME="agv-repo-$(date +%s)"
agv create --include devtools --include gh --start "$NAME"
agv ssh "$NAME" -- 'gh repo clone org/myrepo ~/myrepo'
```

The `gh` mixin auto-authenticates via `gh auth login --with-token` if
either token is set on the host. If neither is set, agv prints a manual
setup line telling the user to run `gh auth login` themselves; the agent
should surface that to the user.

For SSH-key-based access (works for any git host, not just GitHub):

```bash
export SSH_KEY="$HOME/.ssh/id_ed25519"

NAME="agv-repo-$(date +%s)"
agv create --include devtools --include ssh-key --start "$NAME"
```

The `ssh-key` mixin copies the private key in, fixes permissions, and
derives the matching `.pub` via `ssh-keygen -y` — the user only has to
provide one path.

### Browser-based desktop (for the user, not for you)

`agv gui <vm>` is a **host-side convenience that opens the user's
default browser** at the VM's noVNC URL. It's for the user to interact
with — running it from the agent's perspective gives you nothing, since
the opened browser window is on the user's host, not in your context.

Use this recipe when the user explicitly asks for a graphical
environment (browser testing, GUI app screenshots, watching a long
process render).

```bash
NAME="agv-desktop-$(date +%s)"
agv create --include devtools --include gui-xfce --spec large --start "$NAME"

# Hand the URL to the user. --no-launch skips the browser launch and
# just prints the URL, which is friendlier for non-interactive contexts
# (e.g. you're SSHed into the user's machine and there's no DISPLAY).
agv gui --no-launch "$NAME"
# → prints something like:
#     VM:   agv-desktop-...
#     URL:  http://127.0.0.1:12345/vnc.html?autoconnect=1&resize=scale
```

Tell the user to either run `agv gui $NAME` on their host (which opens
the URL in their default browser) or visit the URL directly.

The auth boundary is the SSH tunnel (gated by the VM's ed25519 key); the
in-guest VNC runs without a password — the URL alone is the credential.

#### When *you* want visual access too

Most agents can't usefully interact with the noVNC URL themselves —
they don't have a browser. A few exceptions:

- **You have a controllable browser** (Anthropic's computer-use API,
  Perplexity's browser tools, Playwright/Puppeteer in your toolchain):
  you can navigate to the URL, render the noVNC canvas, and interact
  with it like any other web page. Treat it the same as any other
  remote desktop you'd open in a browser.
- **You only need screenshots, not interaction**: SSH into the VM
  and run something headless inside it (`scrot`, `import`, `headless
  chromium --screenshot`). The `gui-xfce` mixin isn't needed for that
  path; a regular VM with whatever screenshot tool you want works.
- **You want to script keypresses/clicks against the desktop**: SSH
  in and use `xdotool` against the X server inside the VM. Again,
  doesn't require going through the noVNC URL.

If neither of those fits, the GUI mixin is for the user, not for you.

### Long-running, multi-step work

If the task spans more than one session:

```bash
agv create --include devtools --include claude --spec large --start agv-project-x
# … work in the VM …
agv suspend agv-project-x         # save full state to disk, free host RAM
# later:
agv resume agv-project-x          # back to exactly where you left off
```

Suspended VMs use only disk (the qcow2 holds RAM + device state).
`agv resume` is much faster than re-creating.

## Output formats

For human-facing output, agv prints tables and friendly text. For parsing,
use `--json`:

```bash
agv ls --json
agv inspect <name> --json
```

**Don't parse the human-readable table output.** It's not stable across
versions; `--json` is the contract.

## Common pitfalls

- **Boot takes 30+ seconds** on first create. Don't poll fast; the
  `--start` flag blocks until provisioning is done.
- **Auth env vars must be set before `agv create`**, not after. They're
  template-expanded at create time and baked into the saved instance
  config (`~/.local/share/agv/instances/<name>/config.toml`). Setting
  `ANTHROPIC_API_KEY` *after* create won't flow into the VM unless you
  `agv destroy` and `agv create` again — or `agv start --retry` from a
  partially-failed state.
- **A `broken` VM can only be destroyed.** If `agv create` fails partway,
  the VM enters `broken` status with QEMU left running so the user can
  SSH in to debug. `agv start --retry <name>` resumes from the failed
  step. Don't try to `agv start` a broken VM normally.
- **Mixin compatibility.** Each mixin declares which OS families it
  supports. `--image fedora --include devtools` works (devtools has a
  fedora section); some mixins are debian-only. agv fails fast with a
  clear message; don't paper over it, fix the include list or the image.
- **Suspending a VM with active SSH sessions** — agv handles it cleanly,
  but any in-flight commands die. Finish work before suspending.
- **Concurrent `agv create` calls** — should be safe across VMs (each
  VM has its own instance dir), but the image cache is shared. Don't
  spawn five creates simultaneously if any need to download a fresh
  base image; let the first finish, then fan out.

## Communicating with the user

When you create a VM the user didn't explicitly ask for:

- Tell them the name you used.
- Tell them how to access it (`agv ssh <name>` or `agv gui <name>`).
- Say explicitly whether you'll destroy it when done, or if it's theirs.
- Surface any `manual_steps` agv printed (e.g. "you need to run
  `claude /login` inside the VM").

When the user asks "did you finish?":

- If you created any VMs, list them by name and current state.
- Recommend destroying ones they don't need.

## Read the VM's own context

Every agv VM ships a short markdown file at `~/.agv/system.md` listing
the OS family, user + sudo capability, and every mixin applied. Claude
Code loads it automatically via `~/.claude/CLAUDE.md`. Read it first
when you start working inside a VM — it'll tell you what's installed
and any non-obvious wiring (e.g. "docker service enabled, user is in
the docker group").

## Cheat-sheet of useful options

```
agv create --start                    # boot + provision after create
agv create --include <mixin>          # repeatable; mixins compose
agv create --image <ubuntu|debian|fedora>
agv create --spec <small|medium|large|xlarge>
agv create --memory 4G --cpus 2 --disk 20G
agv create --env-file <path>          # explicit .env location
agv create --interactive              # step through provisioning (debugging)
agv create --if-not-exists            # succeed if the VM is already there
agv create --json                     # parseable post-create state (use with --if-not-exists for retries)
agv create --force                    # bypass the host-RAM capacity check

agv start --retry <name>              # resume after a broken provision
agv start --interactive <name>

agv ssh <name> -- <cmd>               # non-interactive; respect quoting
agv ssh <name> -A                     # forward host ssh agent (interactive only)

agv cp <name> :path/in/vm ./local     # copy out
agv cp <name> ./local :path/in/vm     # copy in
agv cp <name> -r :dir ./local         # recursive

agv forward <name> 8080               # add a port forward (host:8080 → guest:8080)
agv forward <name> --list
agv forward <name> --stop 8080

agv gui <name>                        # USER-FACING: opens host browser at the VM's noVNC URL
agv gui --no-launch <name>            # just print the URL; safe to run from a non-TTY context

agv suspend <name>
agv resume <name>

agv destroy <name>                    # confirmation prompt
agv destroy --force <name>            # no prompt; for cleanup scripts
```

`agv --help` and `agv <command> --help` are the authoritative reference.
