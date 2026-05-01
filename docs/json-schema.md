# JSON output and exit codes

`agv` exposes a machine-readable interface for AI agents and scripts:

- Commands that touch state emit structured JSON when called with `--json`.
- The exit code is part of the contract — distinct codes signal distinct
  failure modes so callers can branch without parsing stderr.

This page is the **stability contract** for both. Across the 0.x series:

- **Additions are backwards-compatible.** New JSON keys, new exit codes,
  new optional fields can land in any minor or patch.
- **Removals and renames need a major bump.** A future 1.0 might rename
  `ssh_port` or shuffle the exit-code namespace; before then, neither
  changes.

Schema-pin tests in the codebase enforce this — a rename or removal
fails CI.

---

## `--json` outputs

### `VmStateReport`

The shape returned by every command that observes or mutates a single
VM. Used by:

- `agv create --json` (with `created: true` on a fresh create, `false`
  when `--if-not-exists` short-circuited)
- `agv inspect <name> --json`
- `agv start --json <name>`
- `agv stop --json <name>`
- `agv suspend --json <name>`
- `agv resume --json <name>`
- `agv rename --json <old> <new>` (with the new name)

`agv ls --json` returns a JSON array of these — one entry per VM, in
the same order as the human-readable output.

```json
{
  "name": "myvm",
  "status": "running",
  "created": true,
  "ssh_port": 50121,
  "user": "agent",
  "memory": "8G",
  "cpus": 4,
  "disk": "40G",
  "mixins_applied": ["devtools", "claude"],
  "manual_steps": [
    {
      "name": "claude",
      "steps": ["Run `claude /login` ..."]
    }
  ],
  "config_manual_steps": ["Configure VPN before starting work."],
  "data_dir": "/Users/u/.local/share/agv/instances/myvm",
  "labels": {
    "session": "abc123",
    "needs-cleanup": ""
  },
  "forwards": [
    {"host": 8080, "guest": 8080, "origin": "config", "alive": true}
  ],
  "idle_suspend": {
    "minutes": 30,
    "load_threshold": 0.2,
    "watcher_pid": 4242,
    "watcher_alive": true
  }
}
```

| Field | Type | Notes |
|---|---|---|
| `name` | string | VM name (also the instance directory name) |
| `status` | string | One of: `creating`, `configuring`, `running`, `stopped`, `suspended`, `broken` |
| `created` | bool | `true` only from `agv create` when it actually created the VM; `false` from every other command and from `create --if-not-exists` short-circuits |
| `ssh_port` | uint16 \| null | Present (non-null) only when status is `running`. Always on `127.0.0.1` |
| `user` | string | The VM's default user (default: `agent`) |
| `memory` | string | Configured memory (e.g. `"8G"`) |
| `cpus` | uint32 | Configured vCPU count |
| `disk` | string | Configured max disk size (e.g. `"40G"`) |
| `mixins_applied` | string[] | Mixins applied at create time, in merge order |
| `manual_steps` | object[] | Per-mixin manual steps the human invoker still needs to do. Each has `{name: string, steps: string[]}`. Empty array, never omitted |
| `config_manual_steps` | string[] | Top-level manual steps from the user's `agv.toml`. Empty array, never omitted |
| `data_dir` | string | Absolute path to `~/.local/share/agv/instances/<name>/` |
| `labels` | object<string,string> | Free-form `key=value` metadata set at create time. Always present, even when empty. agv doesn't interpret these — they're for callers (often agents) to tag VMs by session/purpose/etc. The `agv.*` namespace is unreserved today; see CHANGELOG if that ever changes |
| `forwards` | object[] | Snapshot of active port forwards for this VM. Each entry is a `ForwardJson` (see below). Empty array when no forwards are active. Read without sweeping `forwards.toml`, so an entry with `alive: false` indicates a stale supervisor that the next `agv forward --list` would clean up |
| `idle_suspend` | object \| null | Auto-suspend status. `null` when `idle_suspend_minutes == 0` (the default). When set, an `IdleSuspendStatus` object (see below) — the watcher's configured thresholds plus its PID and liveness |

#### `IdleSuspendStatus` (sub-shape of `VmStateReport.idle_suspend`)

```json
{
  "minutes": 30,
  "load_threshold": 0.2,
  "watcher_pid": 4242,
  "watcher_alive": true
}
```

| Field | Type | Notes |
|---|---|---|
| `minutes` | uint32 | Configured `idle_suspend_minutes`. Always `> 0` when this object is present (the parent field is `null` for the disabled case) |
| `load_threshold` | float | Configured `idle_load_threshold` (default `0.2`) — the 5-min loadavg below which the guest counts as idle |
| `watcher_pid` | uint32 \| null | The watcher supervisor's PID, or `null` if `<instance>/idle_watcher.pid` doesn't exist (watcher hasn't started yet, or already exited) |
| `watcher_alive` | bool | Whether the recorded PID is still a running process. `false` distinguishes "configured but no monitor active" from `null` PID. Both cases mean the VM is not currently being watched |

### `DestroyReport`

Returned by `agv destroy --json`. Distinct shape from `VmStateReport`
because the VM no longer exists — no instance dir to read state from.

```json
{
  "name": "myvm",
  "destroyed": true
}
```

| Field | Type | Notes |
|---|---|---|
| `name` | string | The VM that was destroyed |
| `destroyed` | bool | Always `true` (any failure surfaces as a non-zero exit before this is emitted) |

### `ResourceReport`

Returned by `agv resources --json`. Two top-level objects: host capacity
and agv's allocation.

```json
{
  "host": {
    "total_memory_bytes": 51539607552,
    "used_memory_bytes": 38456000512,
    "cpus": 14,
    "data_dir_free_bytes": 577969544551
  },
  "allocated": {
    "running_memory_bytes": 8589934592,
    "running_cpus": 4,
    "running_count": 1,
    "total_memory_bytes": 8589934592,
    "total_cpus": 4,
    "total_disk_bytes": 42949672960,
    "total_count": 1
  }
}
```

`host` fields:

| Field | Type | Notes |
|---|---|---|
| `total_memory_bytes` | uint64 | Physical RAM, bytes |
| `used_memory_bytes` | uint64 | RAM the kernel reports as in-use, bytes. Reported instead of "free" because sysinfo's free reading is unreliable on macOS — subtract from total for an estimate |
| `cpus` | uint32 | Logical CPU count |
| `data_dir_free_bytes` | uint64 | Free bytes on the partition holding `~/.local/share/agv/` |

`allocated` fields:

| Field | Type | Notes |
|---|---|---|
| `running_memory_bytes` | uint64 | Sum of `memory` across VMs in `running` / `configuring` / `creating` states |
| `running_cpus` | uint32 | Sum of `cpus` across the same set |
| `running_count` | uint32 | Number of VMs in those states |
| `total_memory_bytes` | uint64 | Sum of `memory` across every known VM |
| `total_cpus` | uint32 | Sum of `cpus` across every known VM |
| `total_disk_bytes` | uint64 | Sum of declared disk sizes across every VM (qcow2 max sizes — actual usage is lower because of copy-on-write) |
| `total_count` | uint32 | Total VMs known to agv |

### List-like read commands

The following commands emit a JSON array (or object, for `agv doctor`) when called with `--json`. Each shape is a separate stability contract — additions OK across the 0.x series, removals/renames need a major bump.

#### `agv images --json`

Array of image and mixin entries (built-ins plus any user-provided files in `<data_dir>/images/`).

```json
[
  {"name": "ubuntu-24.04", "type": "image", "built_in": true,  "path": null},
  {"name": "claude",       "type": "mixin", "built_in": true,  "path": null},
  {"name": "myimage",      "type": "image", "built_in": false, "path": "/Users/u/.local/share/agv/images/myimage.toml"}
]
```

| Field | Type | Notes |
|---|---|---|
| `name` | string | Image or mixin name |
| `type` | string | `"image"` (full base image) or `"mixin"` (overlays files / setup / provision steps) |
| `built_in` | bool | `true` for entries baked into the binary |
| `path` | string \| null | Absolute path to the user-provided file; `null` for built-ins |

#### `agv specs --json`

Array of hardware-spec entries (built-ins plus any user-provided files in `<data_dir>/specs/`).

```json
[
  {"name": "small",  "memory": "4G",  "cpus": 2, "disk": "20G", "built_in": true, "path": null},
  {"name": "medium", "memory": "8G",  "cpus": 4, "disk": "40G", "built_in": true, "path": null}
]
```

| Field | Type | Notes |
|---|---|---|
| `name` | string | Spec name (e.g. `small`, `medium`) |
| `memory` | string | Memory allocation, e.g. `"8G"` |
| `cpus` | uint32 | Virtual CPU count |
| `disk` | string | Disk size, e.g. `"40G"` |
| `built_in` | bool | `true` for entries baked into the binary |
| `path` | string \| null | Absolute path to the user-provided file; `null` for built-ins |

#### `agv template ls --json`

Array of saved-template entries. Empty array when no templates exist.

```json
[
  {
    "name": "claude-base",
    "source_vm": "claude-source",
    "memory": "8G",
    "cpus": 4,
    "disk": "40G",
    "dependents": ["claude-vm-1", "claude-vm-2"]
  }
]
```

| Field | Type | Notes |
|---|---|---|
| `name` | string | Template name |
| `source_vm` | string | Name of the VM the template was created from |
| `memory` | string | Default memory for VMs cloned from this template |
| `cpus` | uint32 | Default CPU count |
| `disk` | string | Backing-disk size |
| `dependents` | string[] | Names of existing VMs whose disk is backed by this template. Always present, possibly empty |

#### `agv cache ls --json`

Array of cached-image entries. Empty array when the cache is empty.

```json
[
  {"filename": "ubuntu-24.04-arm64.img", "size": 345678901, "in_use": true},
  {"filename": "fedora-43-aarch64.qcow2", "size": 412300000, "in_use": false}
]
```

| Field | Type | Notes |
|---|---|---|
| `filename` | string | File in the image cache directory |
| `size` | uint64 | File size in bytes |
| `in_use` | bool | `true` when at least one VM's disk references this file as a backing image |

#### `agv forward <name> --list --json`

Array of active forwards on a running VM. Empty array when no forwards are active. The same per-entry shape (`ForwardJson`) appears as the `forwards` field of `VmStateReport`.

```json
[
  {"host": 8080, "guest": 8080, "origin": "config", "alive": true},
  {"host": 5432, "guest": 5432, "origin": "adhoc",  "alive": true}
]
```

| Field | Type | Notes |
|---|---|---|
| `host` | uint16 | Host port on `127.0.0.1` |
| `guest` | uint16 | Guest port the forward terminates at |
| `origin` | string | One of: `"config"` (declared in `agv.toml`), `"adhoc"` (added at runtime via `agv forward`), `"auto"` (provisioned by an `[auto_forwards.<name>]` mixin entry) |
| `alive` | bool | Whether the supervisor process for this forward is still running. `agv forward --list` sweeps dead entries before serializing, so `--list` always returns `true`. `VmStateReport.forwards` doesn't sweep, so a stale entry surfaces as `false` |

The supervisor PID tracked internally is intentionally not exposed — it's an implementation detail of how agv keeps the SSH tunnel alive.

#### `agv doctor --json`

Object with the dependency check results. Always emits the same keys (no omissions for missing dependencies — a missing tool surfaces as `found: false`).

```json
{
  "ok": false,
  "issues": 1,
  "checks": [
    {"name": "qemu-system-aarch64", "found": true},
    {"name": "qemu-img",            "found": true},
    {"name": "ssh",                 "found": true},
    {"name": "ssh-keygen",          "found": true},
    {"name": "scp",                 "found": true},
    {"name": "hdiutil",             "found": false}
  ],
  "ssh_include_installed": true
}
```

| Field | Type | Notes |
|---|---|---|
| `ok` | bool | `true` iff every check passed (i.e. `issues == 0`) |
| `issues` | uint32 | Count of failed dependency checks. Does not factor in `ssh_include_installed` — the include is best-effort, not required |
| `checks` | object[] | One entry per dependency, in display order. Each has `{name: string, found: bool}` |
| `ssh_include_installed` | bool \| null | `true` if the agv-managed Include line is present in `~/.ssh/config`; `null` when the host config could not be read |

The check `name` field is human-oriented and may be a slash-joined alternates list (e.g. `"mkisofs / genisoimage"` on Linux); don't pattern-match on it as if it were a stable identifier.

---

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Success |
| `1` | Generic / unexpected failure (config error, QEMU crash, network error, etc.) |
| `2` | Usage error — clap rejected the arguments. Typically: unknown subcommand, missing required arg, unknown flag |
| `10` | VM (or template) already exists. Try `--if-not-exists` on `agv create` or `agv destroy` first |
| `11` | VM, template, image, or include not found. Check `agv ls` |
| `12` | VM is in the wrong state for the operation, or a template still has VMs depending on it |
| `20` | Host RAM is over-committed; `agv create --start` refused the boot. Stop or destroy a running VM first, or pass `--force` to override |

Codes are stable across the 0.x series. Future minor versions may add new
codes (e.g. distinguishing image-download failures from generic `1`); a
future 1.0 might shuffle the namespace.

### Catching specific codes

In bash:

```bash
agv create myvm
case $? in
  0)  ;; # success
  10) echo "myvm already exists; reusing"; agv inspect myvm ;;
  20) echo "host is full; refusing to start"; exit 1 ;;
  *)  echo "unexpected failure"; exit 1 ;;
esac
```

In Python:

```python
import subprocess
result = subprocess.run(["agv", "create", "myvm", "--json"], capture_output=True)
if result.returncode == 10:
    # already exists — branch on existing state
    ...
```

---

## Things that don't have `--json` yet

Commands left out of the JSON contract:

- `agv config show` — overlaps with `agv inspect --json`'s
  `VmStateReport`; pinning the full resolved-config schema is a bigger
  commitment than the rest. Will land if concrete demand shows up.
- `agv ssh`, `agv cp` — pass-through commands; output is whatever the
  user's command / scp produced. Adding `--json` would require
  re-defining the I/O model.
- `agv gui` — opens the user's browser; the URL line is parseable as
  text already (`agv gui --no-launch <vm>` prints just the URL).
- `agv init`, `agv doctor --setup-ssh / --remove-ssh`,
  `agv config set`, `agv cache clean`, `agv template create`,
  `agv template rm` — produce side effects rather than data.

When these gain `--json` support, their shapes will land here.
