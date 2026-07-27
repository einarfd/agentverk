# Remote IDE setup

Connect your IDE to a running agv VM for a full remote development experience.
Most IDEs use SSH under the hood, so the one-time setup is the same for all of them.

## One-time setup

Run this once to let agv manage your SSH config:

```sh
agv doctor --setup-ssh
```

This adds an `Include` line to `~/.ssh/config` pointing to an agv-managed config
file. agv automatically maintains a `Host` entry for each running VM, so they are
accessible by name from `ssh`, `scp`, `rsync`, and any IDE with SSH support.

```sh
ssh myvm                 # connect by name — no port or key needed
scp file.txt myvm:~/     # copy files using standard scp
```

The entries are managed automatically:
- `agv start` / `agv create --start` — adds the VM's entry
- `agv stop` / `agv destroy` — removes it

To undo the setup: `agv doctor --remove-ssh`

## Zed

Zed reads `~/.ssh/config` natively. After `agv doctor --setup-ssh`, running VMs
appear in **File → Open Remote** with no additional configuration.

## VS Code / Cursor

1. Install the **Remote - SSH** extension (`ms-vscode-remote.remote-ssh`).
2. Open the Command Palette → **Remote-SSH: Connect to Host...**
3. Select your VM by name (e.g. `myvm`).
4. VS Code opens a new window connected to the VM. Open a folder and start working.

## JetBrains (PyCharm, IntelliJ, GoLand, etc.)

**Via JetBrains Gateway:**

1. Open JetBrains Gateway (bundled or standalone).
2. Select **SSH Connection** → **New Connection**.
3. Enter the VM name as the host (e.g. `myvm`), user `agent`.
   The SSH config provides the port and key automatically.
4. Choose the IDE and project directory inside the VM.

**Via the IDE directly:**

1. File → Remote Development → SSH.
2. Enter `myvm` as the host. Connection details are filled from SSH config.

## Browser-based desktop

If you need an actual graphical desktop (running a browser inside the VM,
using GUI tools, etc.) rather than just editing files, add the `gui-xfce`
mixin and use `agv gui <name>` to open an XFCE desktop in your host
browser. The same SSH-key auth boundary applies. See
[`docs/config.md`](config.md#desktop--gui-access) for details.

## Neovim / terminal editors

Just SSH in:

```sh
agv ssh myvm
```

Or use Neovim's built-in remote editing:

```sh
nvim scp://myvm//home/agent/project/file.py
```

## Port forwarding for web UIs

If your project runs a web server inside the VM, forward the port. `agv forward`
returns immediately — the forward lives on QEMU, not on your terminal:

```sh
agv forward myvm 8080              # host:8080 → VM:8080
agv forward myvm 3000:8080         # host:3000 → VM:8080
```

Then open `http://localhost:8080` (or `3000`) in your browser.

Forwards are persistent: the command above saves the mapping to the VM's config
as well as applying it, so it comes back on every later `agv start` / `agv
resume`. It works on a stopped VM too — the change lands in the config and
takes effect on the next start.

Manage what's active:

```sh
agv forward myvm --list            # show everything currently forwarded
agv forward myvm --rm 8080         # remove one, config included
agv forward myvm --rm              # remove every forward (asks first)
```

For a tunnel you want only until the VM stops, add `--temporary` — it skips
the config write. It works on removals too, so `agv forward myvm --rm 8080
--temporary` drops the tunnel now but leaves the config entry to return on the
next start. `agv forward myvm --reapply` puts back any config forward that
isn't currently running, which is the undo for that.

You can equally declare forwards up front in `agv.toml`:

```toml
[[forwards]]
host = 8080

[[forwards]]
host = 3000
guest = 8080
```

A compact `forwards = ["8080", "3000:8080"]` string list works too, but it must
sit above the first `[section]` in the file — `[[forwards]]` blocks can go
anywhere. See `docs/config.md#forwards` for the full syntax and `agv forward
--help` for more options.

## Copying files

Use `agv cp` to copy files to and from a running VM:

```sh
agv cp myvm :~/file.txt ./              # download from VM
agv cp myvm ./file.txt :~/              # upload to VM
agv cp myvm -r :~/project/ ./local/     # recursive download
```

See `agv cp --help` for more details.

## Troubleshooting

**VM not showing up in IDE?**

- Make sure the VM is running: `agv ls`
- Check the setup: `agv doctor` (should show "SSH config Include: ✓ installed")
- Verify the entry exists: `ssh -G myvm` should show the connection details

**Connection refused?**

- The VM may still be booting. Wait a few seconds and retry.
- Check that SSH is ready: `agv ssh myvm -- echo ok`

**Wrong user or key?**

- The managed config uses the VM's configured user (default: `agent`) and
  the agv-generated key. These are set automatically — no manual config needed.
