# Agent ergonomics: rough audit

A list of ways agv could be friendlier to AI agents driving the CLI,
collected while drafting [`skills/agv/SKILL.md`](../skills/agv/SKILL.md).
Each item ends with a rough effort tag (S/M/L). None of these are
release-blocking; the skill itself works against agv as it stands today.
Treat this as a backlog rather than a roadmap — pick what's worth doing
based on what feedback the skill surfaces in practice.

## Resource awareness (`agv resources`) ✓ shipped

> An AI agent calling `agv create --memory 16G` against a host with 8G
> free will fail at QEMU spawn time with an opaque kernel-level error.
> A human picks size by feel because they know the machine; an agent
> doesn't.

**Shipped (post-0.2.2):**

- **`agv resources`** — prints host RAM (used / total), CPU count,
  free disk in the data dir partition, plus agv's allocation (running
  + total VMs, with RAM / vCPUs / declared disk). `--json` for
  machine-readable output. Implemented via the `sysinfo` crate for
  cross-platform memory probing.
- **`agv create --start` capacity check** — refuses to boot when
  memory of new VM + already-running VMs would exceed 90% of host
  total RAM. Error message names the numbers and the recovery options
  (`agv ls` / `agv stop`, or `--force`). Doesn't fire on plain
  `agv create` (no boot, no host RAM allocated).

**Still pending:**

- Adding allocated resource fields to `agv ls --json` per-VM output
  is still on the list; lumping it into the broader "stable JSON
  schema contract" work below rather than doing it piecemeal.

Effort actual: ~250 LOC + tests. Took a single session.

## Stable, documented `--json` contract

> Agents will parse `--json` output. Any churn there breaks scripts.

State of play (as of post-0.2.2):

- ✓ **`agv create --json`**: emits `VmStateReport`.
- ✓ **`agv ls --json`**: emits `[VmStateReport, ...]`.
- ✓ **`agv inspect --json`**: emits `VmStateReport`.
- ✓ **`agv resources --json`**: emits `ResourceReport`.
- ✓ **`agv start / stop / suspend / resume / rename --json`**: each
  emits the post-action `VmStateReport`.
- ✓ **`agv destroy --json`**: emits a distinct `DestroyReport`
  (`{name, destroyed}`) since the VM no longer exists.
- ✓ Schema-pin tests for both `VmStateReport` and `DestroyReport`.
  Renaming or removing a key fails loudly; silent additions are
  caught.
- ✓ Removed the unused global `Cli.json` flag.
- ✓ Integration test sweeps every lifecycle verb to make sure
  `--json` is registered (catches "I forgot the flag on a new
  command" regressions).

**Shipped (3c, post-0.2.3):**

- ✓ **List-like informational commands**: `agv images`, `agv specs`,
  `agv template ls`, `agv cache ls`, `agv forward --list`, and
  `agv doctor` all accept `--json` and emit a documented,
  schema-pinned shape (each shape is a separate `0.x` stability
  contract; additions OK, removals/renames need a major bump).
  Schema-pin tests live next to each struct (`ForwardJson`,
  `ImageJson`, `SpecJson`, `TemplateInfo`, `CacheEntry`,
  `DoctorReport`) and an integration sweep in `tests/cli_test.rs`
  verifies clap accepts `--json` on each verb (catches "I forgot
  the flag on a new list command" regressions).
- ✓ **`docs/json-schema.md`** — every `--json` shape is documented
  with field-by-field tables, plus a "things that don't have
  `--json` yet" section listing the deliberate omissions
  (`config show`, side-effect commands, pass-through commands).

Still pending:

- **`agv config show --json`** — overlaps with `inspect --json`'s
  `VmStateReport`; pinning the full resolved-config shape (mixin
  list, files, setup, provision steps) is a bigger commitment than
  the rest. Defer until a concrete need surfaces.

### Side note — slow boot tests should validate JSON, not text

Today `tests/create_test.rs` validates outcomes by parsing
human-readable output (status strings, log file presence, etc.).
Once the lifecycle verbs all emit `--json`, those slow boot tests
should switch to asserting on the JSON `VmStateReport` instead — it's
a more stable contract, easier to query, and dogfoods the agent path
the skill recommends. Separate work item; ship after 3b lands.

## Idempotent `agv create` ✓ shipped

**Shipped:** `--if-not-exists` flag on `agv create`. When the VM is
already present the command exits 0 with no changes; with `--json`,
it prints the existing VM's state (with `created: false`) so the agent
can still act on the reply. Doesn't auto-start an existing stopped VM
— that stays an explicit `agv start` to keep the semantics narrow.

Note: chose this over the alternative `agv ensure <name>` verb. The
flag composes naturally with every other create flag and was a
single-flag change rather than a new sibling verb.

Effort actual: ~50 LOC.

## Distinct, documented exit codes ✓ shipped

**Shipped:** the agent-relevant codes are in place and documented in
`docs/json-schema.md`. The shape:

- **0** success
- **1** generic / unexpected error (catch-all)
- **2** clap usage error (unknown subcommand, missing arg, bad flag)
- **10** VM or template already exists
- **11** VM, template, image, or include not found
- **12** VM in wrong state, or template has dependents
- **20** host capacity refused (`agv create --start` over the 90% RAM
  threshold without `--force`)

The mapping lives in `src/error.rs::exit_code_for`, walks the anyhow
chain, and falls through to `1` for unstructured failures. The
resource-capacity refusal now returns a structured
`Error::HostCapacity` variant instead of `anyhow::bail!()` — needed so
the chain-walker can see it.

**Not shipped (deliberately, for now):**

- Codes `30` (image download/checksum) and `40` (provisioning) from the
  original proposal weren't added. They'd touch a lot of error sites
  for marginal agent value — a generic `1` with an explanatory error
  message is fine for now. Adding them later is backwards-compatible.

Tests:

- Unit tests in `src/error.rs` cover the mapping for every variant
  and the chain-walking behaviour.
- Integration tests in `tests/cli_test.rs` verify exit code 11 on
  not-found commands and exit code 2 from clap.

Effort actual: ~150 LOC + tests + the schema doc.

## Labels for session tracking ✓ shipped

**Shipped:**

- `agv create --label k=v` (repeatable; bare `--label foo` is shorthand
  for `foo=""`). Stored in the saved `ResolvedConfig` (and so in the
  per-instance `config.toml`).
- `agv ls --label k=v` filters; multiple filters AND together; bare-key
  matches any value.
- `agv ls --labels` shows the labels column in human output (hidden by
  default to keep the table compact).
- `agv destroy --label k=v` does bulk destroy by selector. Lists matched
  VMs and prompts unless `-y` or `--json`. Refuses running matches
  unless `--force`.
- `agv inspect` shows a Labels: section in human output (only when
  there are any).
- `VmStateReport` gains a `labels` field, documented in
  `docs/json-schema.md`. Always present in JSON, even when empty.
- `agv.*` namespace **not** reserved — fully user-owned today; will be
  reserved in a future minor only if we actually need built-in
  agv-managed labels.

Convention an agent should follow: `--label session=<short-id>` on
every create. Then a single `agv destroy --label session=<short-id>
--force` cleans up everything that session created, including running
VMs. The skill recipe shows this pattern.

Effort actual: ~300 LOC including tests + docs.

## `agv create --json` output on success ✓ shipped

**Shipped:** `--json` on `agv create` emits a `VmStateReport` object
on success. Fields: `name`, `status`, `created`, `ssh_port`, `user`,
`memory`, `cpus`, `disk`, `mixins_applied`, `manual_steps`,
`config_manual_steps`, `data_dir`. Same shape will be reused by
`agv inspect --json` when that lands (audit item: stable JSON
contract, below). Stable over the 0.x minor series — additions are
backwards-compatible, removals/renames need a major bump.

The `created` field distinguishes "agv create just created this" from
"`--if-not-exists` short-circuited because the VM was already there",
so an agent can branch on whether their session's VM is fresh.

Effort actual: ~80 LOC including the shared report struct.

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

## Concurrency audit ✓ shipped

**Audit findings:** `<data_dir>/ssh_config` had a real read-modify-write
race — two parallel `agv start` calls against different VMs would each
read, modify, and write back; whichever wrote second clobbered the
first writer's Host entry. Image cache downloads were collision-safe
via PID+nanos partial filenames but two processes would still both
download the same image (wasted bandwidth, not corruption). Per-VM
instance directories are isolated, so no lock needed there.

**Shipped:**

- New `src/locks.rs` — `flock(2)`-based advisory cross-process locks,
  using rustix (already a dep, no new crate). RAII `LockGuard`
  releases on drop, including on panic. Acquire is delegated to
  `tokio::task::spawn_blocking` so a contended lock doesn't park the
  async runtime's worker thread.
- `ssh_config::add_entry` and `remove_entry` hold the lock for the
  full read-modify-write — fixes the race. Lockfile is sibling
  `<data_dir>/ssh_config.lock`.
- `image::ensure_cached` uses the same lock pattern with
  double-checked existence, so concurrent fetches of the same image
  serialise on the download instead of duplicating it.
- `AGENTS.md` has a "Concurrency contract" section documenting the
  agreement: two `agv` commands against different VMs are safe;
  against the same VM they're not (no per-instance locking).

Tests:

- `src/locks.rs` unit tests use `spawn_blocking` (so they go through
  the actual `flock` syscall path) and verify the lock serialises
  4 concurrent acquirers via a counter that would otherwise
  interleave.
- `tests/cli_test.rs::parallel_resources_invocations_all_succeed`
  spawns 8 `agv` subprocesses against the same data dir, verifying
  the binary doesn't deadlock or corrupt under cross-process
  concurrency.
- Real cross-process flock semantics for ssh_config writes weren't
  tested directly because writing to ssh_config requires booting a
  VM (slow boot territory). The locks unit tests cover the
  underlying mechanism; the `add_entry`/`remove_entry` callers are
  trivially correct given the lock.

Effort actual: ~150 LOC (module + integrations + tests + docs).

## Better naming hint in `agv create --help` ✓ shipped

**Shipped:** the `<NAME>` doc on `CreateArgs` now suggests
`agv-<task>-<short-id>` and points at `--label session=<id>` for the
cleanup pattern. Single-paragraph form because clap derive only
renders the first paragraph of the doc-comment for positional args
(flags get the multi-paragraph long-help treatment, positionals
don't).

Effort actual: 4 lines.

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
