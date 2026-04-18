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

Use a GitHub [fine-grained personal access token](https://github.com/settings/personal-access-tokens)
with the `gh` CLI. The token is passed via a template variable so it never appears in
the config file.

```toml
# agv.toml
[base]
from    = "ubuntu-24.04"
include = ["devtools", "gh"]
spec    = "large"

[[provision]]
run = [
  "gh auth login --with-token <<< '{{GITHUB_TOKEN}}'",
  "gh auth setup-git",                    # configure git credential helper
  "gh repo clone org/myrepo ~/myrepo",
]
```

```sh
# .env  — add to .gitignore, never commit
GITHUB_TOKEN=ghp_...
```

**Token permissions:** Fine-grained tokens let you select exactly which repositories the
token can access and what it can do. For read-only cloning, grant `Contents: Read` on
the target repositories. Add `Pull requests: Read & Write` if the agent needs to open
PRs. Use the minimum permissions needed — especially important when agents are running
code autonomously.

---

## 2. Copy an SSH key into the VM

Copy an existing key from the host via `[[files]]`. The simplest option if you already
use SSH keys for GitHub.

```toml
[[files]]
source = "{{HOME}}/.ssh/id_ed25519"
dest   = "/home/{{AGV_USER}}/.ssh/id_ed25519"

[[files]]
source = "{{HOME}}/.ssh/id_ed25519.pub"
dest   = "/home/{{AGV_USER}}/.ssh/id_ed25519.pub"

[[provision]]
run = """
chmod 600 ~/.ssh/id_ed25519
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

**Step 1 — create the VM:**

```sh
agv create myvm
agv start myvm
```

**Step 2 — get the VM's public key:**

```sh
agv ssh myvm -- cat ~/.ssh/id_ed25519.pub
```

**Step 3 — add it as a deploy key** in the repo's GitHub Settings → Deploy keys.
Tick "Allow write access" if the agent needs to push.

**Step 4 — use it in provisioning:**

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

Forward your local SSH agent into the VM with `agv ssh`. The key never touches the
VM's disk — it stays on your machine.

```sh
agv ssh myvm -A
```

Once inside the VM, git operations will authenticate via your forwarded agent as normal.

**Limitation:** Agent forwarding only works for your interactive shell session. It is
not available during `[[provision]]` steps, which run before you connect. Use a
different approach if you need to clone repos during provisioning.

---

## Dotfiles

Dotfiles are a natural fit for VM provisioning — the same config you maintain for your
own machines works equally well inside an agent VM.

**Option A — clone a dotfiles repo:**

```toml
[[provision]]
run = [
  "gh auth login --with-token <<< '{{GITHUB_TOKEN}}'",
  "gh repo clone org/dotfiles ~/dotfiles && ~/dotfiles/install.sh",
]
```

This is the most flexible option: the install script can set up shell config, git
config, editor settings, and anything else the agent needs.

**Option B — copy files directly:**

```toml
[[files]]
source = "{{HOME}}/.gitconfig"
dest   = "/home/{{AGV_USER}}/.gitconfig"

[[files]]
source = "{{HOME}}/.config/gh"
dest   = "/home/{{AGV_USER}}/.config/gh"
```

Simpler, but requires listing each file individually. Works well for a small set of
config files that don't need an install step.

---

## Security summary for agent use

When an agent is executing code — especially AI-generated code — it may inadvertently
run something that reads credentials from the filesystem or environment.

**Prefer:**
- Deploy keys over personal SSH keys (scoped to one repo, separately revocable)
- Fine-grained PATs with minimal repository and permission scope
- Read-only access where possible — agents rarely need to push

**Avoid:**
- Copying your primary SSH key into a VM that runs untrusted code
- Broad PATs when a scoped token would do
- Storing credentials in the config file itself (use `.env` or environment variables)

Destroying a VM with `agv destroy` removes the disk, the generated SSH key, and all
instance state, so credentials do not persist after the VM is gone.
