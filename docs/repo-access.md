# Accessing private repositories

AI agents often need to clone and push to private repositories. This page covers the
main approaches, their trade-offs, and security considerations — particularly important
when the agent is executing code it may have written itself.

## Approaches at a glance

| Approach | Complexity | Scope | Works during provisioning |
|----------|-----------|-------|--------------------------|
| PAT token via `gh` | Low | Configurable | Yes |
| Copy SSH key | Low | All repos your key accesses | Yes |
| Deploy key | Medium | One repo, read or read-write | Yes |
| SSH agent forwarding | Low | All repos your key accesses | No — interactive only |

---

## 1. PAT token with GitHub CLI (recommended)

Use a [GitHub Personal Access Token](https://github.com/settings/tokens) with the
`gh` CLI. The token is passed via a template variable so it never appears in the
config file.

```toml
# agv.toml
[base]
from    = "ubuntu-24.04"
include = ["devtools", "gh"]
spec    = "large"

[[provision]]
run = "gh auth login --with-token <<< '{{GITHUB_TOKEN}}'"

[[provision]]
run = "gh repo clone org/myrepo ~/myrepo"

# Configure git credential helper so plain `git` commands work too.
[[provision]]
run = "gh auth setup-git"
```

```sh
# .env  — add to .gitignore, never commit
GITHUB_TOKEN=ghp_...
```

`gh auth setup-git` must run **after** `gh auth login`. It configures git's credential
helper so that `git clone`, `git push`, etc. all authenticate via the stored token
without needing further configuration.

**Token scopes:** For read-only access, `Contents: Read` is sufficient. For push access
or using `gh pr create` and similar, add `Pull requests: Read & Write`.
Use the minimum scopes needed.

---

## 2. Copy an SSH key into the VM

Copy an existing key from the host via `[[files]]`. The simplest option if you already
use SSH keys for GitHub.

```toml
[[files]]
source = "{{HOME}}/.ssh/id_ed25519"
dest   = "/home/agent/.ssh/id_ed25519"

[[files]]
source = "{{HOME}}/.ssh/id_ed25519.pub"
dest   = "/home/agent/.ssh/id_ed25519.pub"

[[provision]]
run = """
chmod 600 /home/agent/.ssh/id_ed25519
ssh-keyscan github.com >> ~/.ssh/known_hosts
"""

[[provision]]
run = "git clone git@github.com:org/myrepo.git ~/myrepo"
```

**Security:** The key lives on the VM's disk. If the agent runs malicious code or the
VM is compromised, the attacker has your full SSH key and access to everything it unlocks.
Prefer a dedicated key or deploy key (see below) for agent use.

---

## 3. Deploy key (recommended for agents)

A deploy key is a per-repository SSH key. It can be read-only or read-write, and is
scoped to a single repo — ideal for agents that should only access specific repositories.

agv generates a unique SSH key for each VM. Use that key as the deploy key so no
secret needs to be passed in at all.

**Step 1 — create the VM first** (provisioning will fail since the repo isn't accessible
yet, but the key is generated):

```sh
agv create myvm
```

**Step 2 — get the VM's public key:**

```sh
agv ssh myvm -- cat ~/.ssh/id_ed25519.pub
```

**Step 3 — add it as a deploy key** in the repo's GitHub Settings → Deploy keys.
Tick "Allow write access" if the agent needs to push.

**Step 4 — provision:**

```sh
agv ssh myvm -- git clone git@github.com:org/myrepo.git ~/myrepo
```

Or re-create the VM with a `[[provision]]` block now that the key is authorised:

```toml
[[provision]]
run = """
ssh-keyscan github.com >> ~/.ssh/known_hosts
git clone git@github.com:org/myrepo.git ~/myrepo
"""
```

Each VM gets its own key, so revoking one VM's access does not affect others.

---

## 4. SSH agent forwarding (interactive sessions only)

Forward your local SSH agent into the VM with `ssh -A`. The key never touches the
VM's disk — it stays on your machine.

```sh
ssh -A -p $(cat ~/Library/Application\ Support/agv/instances/myvm/ssh_port) \
    agent@localhost
```

**Limitation:** Agent forwarding only works for your interactive session. It is not
available during `[[provision]]` steps, which run before you connect. Use a different
approach if you need to clone repos during provisioning.

---

## Dotfiles and git identity

Agents typically need a git identity for commits. Set it in a `[[provision]]` step, or
copy a `.gitconfig` from the host:

```toml
# Option A — copy from host
[[files]]
source = "{{HOME}}/.gitconfig"
dest   = "~/.gitconfig"

# Option B — set inline
[[provision]]
run = """
git config --global user.name  "My Agent"
git config --global user.email "agent@example.com"
"""
```

---

## Security summary for agent use

When an agent is executing code — especially AI-generated code — it may inadvertently
run something that reads credentials from the filesystem or environment.

**Prefer:**
- Deploy keys over personal SSH keys (scoped to one repo, separately revocable)
- Minimal PAT scopes (only the permissions actually needed)
- Read-only access where possible — agents rarely need to push

**Avoid:**
- Copying your primary SSH key into a VM that runs untrusted code
- Full-access PATs when a scoped token would do
- Storing credentials in the config file itself (use `.env` or environment variables)

Destroying a VM with `agv destroy` removes the disk, the generated SSH key, and all
instance state, so credentials do not persist after the VM is gone.
