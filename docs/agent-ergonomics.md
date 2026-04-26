# Agent ergonomics: rough audit

A list of ways agv could be friendlier to AI agents driving the CLI,
collected while drafting [`skills/agv/SKILL.md`](../skills/agv/SKILL.md).
Each item ends with a rough effort tag (S/M/L). None of these are
release-blocking; the skill itself works against agv as it stands today.
Treat this as a backlog rather than a roadmap — pick what's worth doing
based on what feedback the skill surfaces in practice.

## Resource awareness (`agv resources`)

> An AI agent calling `agv create --memory 16G` against a host with 8G
> free will fail at QEMU spawn time with an opaque kernel-level error.
> A human picks size by feel because they know the machine; an agent
> doesn't.

What's missing today:

- No way to query host capacity (RAM, CPU count, free disk in the
  agv data dir partition) from the agv CLI.
- `agv create` doesn't reject or warn on overcommit.
- `agv ls` / `agv inspect` don't show currently-allocated totals.

Proposed additions:

- **`agv resources`** (or `agv host`) — prints host total RAM, currently
  allocated by running VMs, free disk in the data dir partition, vCPU
  count. JSON form via `--json`.
- **`agv create` capacity check** — by default, refuse the create when
  it would push allocated RAM beyond a configurable threshold (say,
  90% of physical RAM); add `--force` to override. Warn on disk too.
- **Allocated resources in `agv ls --json`** — current schema only
  shows status; including memory/cpus/disk per VM lets an agent compute
  totals without parsing each `inspect` separately.

Effort: M. Reading host capacity is platform-specific (sysctl on macOS,
/proc on Linux) but small. The check logic on create is trivial.

## Stable, documented `--json` contract

> Agents will parse `--json` output. Any churn there breaks scripts.

Today: `agv ls --json` and `agv inspect --json` exist; the schema is
not formally documented. Other commands may or may not have `--json`.

Proposed:

- Audit every command an agent might call from a script. At minimum:
  `ls`, `inspect`, `create`, `start`, `stop`, `suspend`, `resume`,
  `destroy`, `forward --list`, `images`, `specs`. Confirm each has
  `--json` or document why not.
- Add a section to `docs/config.md` (or a new `docs/json-schema.md`)
  documenting each command's output schema. Treat the schema as a
  semver-ish contract: additions OK, removals/renames need a major
  version bump.
- For commands that don't naturally produce data (`create`, `start`,
  `destroy`), `--json` should still emit a useful object — name,
  status, any host-side details an agent needs to act on the result
  (e.g. `ssh_port` after `create --start`).

Effort: M. Mostly auditing + documentation; small code changes for
commands that don't currently emit anything in `--json` mode.

## Idempotent `agv create`

> An agent that lost track of session state (crash, interrupted run)
> wants "create-or-resume" semantics, not "fail because it exists".

Today: `agv create <name>` errors with `VmAlreadyExists` if the
instance dir is present.

Proposed:

- **`--if-not-exists`** flag on `create`. With it, an existing VM is
  a successful no-op (exit 0), output is the VM's current state in
  `--json`. Lets an agent always run `agv create --if-not-exists
  agv-session-x --start` without first checking `ls`.
- Alternative spelling: `agv ensure <name> ...` as a sibling verb
  that reads "make sure this VM exists with this config". Cleaner
  but a bigger surface bump.

I lean on `--if-not-exists` — it's smaller and composes with all the
other create flags.

Effort: S. The check already exists; the flag flips the behavior.

## Distinct, documented exit codes

> An agent that gets exit 1 on a `create` doesn't know whether to
> retry, surface to the user, or give up.

Today: most failures are exit 1. Some specific errors might use other
codes; not audited and not documented.

Proposed (rough):

- **0** — success.
- **1** — generic / unexpected error.
- **2** — usage error (bad flags, conflicting options).
- **10** — VM already exists (when not using `--if-not-exists`).
- **11** — VM not found.
- **12** — VM is in the wrong state for the operation (e.g. trying to
  `agv start` a `broken` VM, or `agv suspend` a stopped one).
- **20** — host capacity (when the resource check refuses a create).
- **30** — image download / checksum failure.
- **40** — provisioning failure (the VM is `broken` after this).

Document in `docs/json-schema.md` (or wherever the `--json` contract
lives). Treat as semver-stable.

Effort: M. The error variants exist in `src/error.rs`; mapping them
to distinct exit codes is mechanical. The audit + documentation is
the real work.

## Labels for session tracking

> An agent juggling multiple VMs across a session loses track. There's
> no "I created these" view today.

Proposed:

- **`agv create --label key=value`** (repeatable). Labels are stored
  in the saved instance config and shown in `agv inspect`.
- **`agv ls --label key=value`** filters to matching VMs.
- **`agv destroy --label key=value`** (with confirmation) tears down
  every VM with the label. Useful for "clean up all the VMs my session
  created".

Convention an agent could follow: `--label session=<short-id>` on
every create, plus `--label agv-skill-version=<x>` so future skill
versions know which VMs they own.

Effort: M. New schema field on ResolvedConfig; small CLI plumbing.
Worth pairing with the resource-awareness work since both touch
`agv ls --json`'s schema.

## `agv create --json` output on success

> When `agv create --start` finishes, the agent has no machine-readable
> handoff — it has to `agv inspect` afterwards to learn the SSH port.

Today: success output is human-friendly text via the spinner.

Proposed:

- With `--json`, emit a single JSON object on success with at minimum:
  `{ "name": "...", "status": "running", "ssh_port": 12345,
    "manual_steps": [...] }`.
- The agent can then act immediately without an extra `inspect` round-trip.

Effort: S. Just structured printing.

## Decoupled "wait for ready"

> Today `agv create --start` is a single blocking call. An agent that
> wants to fire-and-forget the create, do other work, then come back,
> can't easily.

Proposed:

- **`agv create --no-wait`** — kick off the create but return as soon
  as QEMU is launched, before provisioning completes. Status will be
  `configuring`.
- **`agv wait <name>`** — block until status is one of {`running`,
  `stopped`, `broken`}. Useful for the agent to come back to.

Useful pattern: agent kicks off three VMs at once with `--no-wait`,
does other work, then `agv wait <each>` in turn.

Effort: M. Provisioning is currently inline in `agv create`; teasing it
apart so a separate `wait` verb can rejoin a backgrounded provision is
a bit of plumbing. Worth checking the existing provision-state machinery
already supports this pattern — it tracks phase + index, which is most
of what's needed.

## Concurrency audit

> Can `agv create vm1` and `agv create vm2` run safely in parallel?
> Probably yes, but I haven't proven it.

Shared state across invocations:

- `~/.local/share/agv/cache/images/` — base image cache. Two creates
  needing the same uncached image would race on the download.
- `~/.local/share/agv/ssh_config` — managed SSH config file.
- `~/.local/share/agv/instances/<name>/` — per-VM, isolated.

Proposed:

- Add file locks around the image cache (advisory lock during download)
  and the managed SSH config update.
- Add a section to `AGENTS.md` documenting the concurrency contract:
  "two `agv` commands against different VMs are safe; against the same
  VM they're not".
- A test that spawns N parallel `agv create` calls (with `--image`
  pointing at a tiny test image) and asserts none corrupt state.

Effort: M. flock or fs2 crate for the locking; a category-2 integration
test is enough to prove it.

## Better naming hint in `agv create --help`

Small one. Today the `--name` help is just "VM name". A line suggesting
"agents: use `agv-<task>-<short-id>` so multiple agents can coexist"
would propagate the convention.

Effort: trivial. Drop in when next touching `cli.rs`.

## Auth env-var timing — runtime warning?

The skill calls out that auth env vars must be set *before* `agv create`,
not after. We could also have `agv create` warn at runtime when an
included mixin's expected env var (e.g. `ANTHROPIC_API_KEY` for the
claude mixin) isn't set:

> Warning: claude mixin included but ANTHROPIC_API_KEY is not set.
> The VM will be created without auth; you'll need to run `claude
> /login` inside the VM.

Effort: S, but maybe noise. The mixin's `manual_steps` already covers
this and the host echo prints it after provision. A pre-flight warning
is duplicate. Skip unless feedback says otherwise.

## Suggested ordering

If we do these in 0.3.0 (one minor bump because the labels + JSON
schema work is borderline-breaking for any existing scripts):

1. **Resource awareness** + `agv resources` — directly addresses the
   "agent doesn't know the host" gap.
2. **`agv create --if-not-exists`** + **`agv create --json` output** —
   small, high-ROI for agent loops.
3. **Distinct exit codes** + **`--json` schema docs** — stabilizes
   the contract before more agents start parsing it.
4. **Labels** — once the schema's documented; cleanest to ship with
   the schema doc as one bundle.
5. **Decoupled wait** — only if real demand shows up. The blocking
   `--start` works fine for the common case.
6. **Concurrency audit + locks** — defensive; do once another item
   forces touching cache.

(1) and (2) are likely small enough to fit a 0.2.x patch; (3)–(6)
deserve a minor bump because they redefine some surfaces.
