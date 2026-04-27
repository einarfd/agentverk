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

Commands left out of the JSON contract as of 0.2.x:

- `agv images`, `agv specs`, `agv template ls`, `agv cache ls`,
  `agv forward --list`, `agv config view`, `agv doctor` — list-like
  informational commands, called rarely. Will land in a future minor
  version when there's concrete demand.
- `agv ssh`, `agv cp` — pass-through commands; output is whatever the
  user's command / scp produced. Adding `--json` would require
  re-defining the I/O model.
- `agv gui` — opens the user's browser; the URL line is parseable as
  text already (`agv gui --no-launch <vm>` prints just the URL).
- `agv init`, `agv doctor --setup-ssh / --remove-ssh` — produce side
  effects rather than data.

When these gain `--json` support, their shapes will land here.
