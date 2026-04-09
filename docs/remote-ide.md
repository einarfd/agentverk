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

**Known issue — JetBrains Toolbox on macOS:** JetBrains Toolbox does not correctly
handle `Include` directives in `~/.ssh/config` when the included path contains
spaces (the agv data dir on macOS is `~/Library/Application Support/agv/`). This
affects Toolbox only — Gateway and the IDEs themselves work fine. On Linux (where
the path is `~/.local/share/agv/`) Toolbox works correctly.

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

If your project runs a web server inside the VM, forward the port:

```sh
agv forward myvm 8080              # VM:8080 → local:8080
agv forward myvm 3000:8080         # VM:8080 → local:3000
```

Then open `http://localhost:8080` (or `3000`) in your browser.
See `agv forward --help` for more options.

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
