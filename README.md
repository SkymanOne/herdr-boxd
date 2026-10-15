# herdr-boxd

Start a [boxd](https://boxd.sh) machine from a snapshot and attach it in [Herdr](https://herdr.dev). Other agents can also use it to run coding subagents inside boxd machines and keep track of them.

It runs on macOS and Linux. The plugin and pane mode need Herdr 0.9.1 or newer. Headless use works without Herdr. You also need a `boxd` login on this computer, or to be inside a boxd VM. Rust and protoc are needed only to build from source when no release binary is available.

## Install

Plugin. Pin a release with `--ref v0.3.0`. If this directory was linked, unlink it first:

```bash
herdr plugin unlink boxd.session
herdr plugin install SkymanOne/herdr-boxd --ref v0.3.0
```

CLI only, on Apple Silicon or Linux x86_64, including a boxd VM:

```bash
curl -fsSL https://raw.githubusercontent.com/SkymanOne/herdr-boxd/main/install.sh | sh
```

Into a VM from this computer:

```bash
boxd machine exec <vm> -- sh -c 'curl -fsSL https://raw.githubusercontent.com/SkymanOne/herdr-boxd/main/install.sh | sh'
```

`HERDR_BOXD_VERSION` selects a release. `HERDR_BOXD_INSTALL_DIR` defaults to `~/.local/bin`.

Development uses a source checkout:

```bash
cargo build --release --locked
herdr plugin link .
```

Bind a key in `~/.config/herdr/config.toml` (`prefix+b` is the sidebar):

```toml
[[keys.command]]
key = "prefix+m"
type = "plugin_action"
command = "boxd.session.menu"
description = "Boxd menu"
```

Then run `herdr server reload-config`.

## From Herdr

The menu lists your plugin machines with their live status. Press a key; no Enter needed. Esc goes back.

| Key | Action |
| --- | --- |
| 1 | Start a session |
| 2 | Set the snapshot name |
| 3 | Save a plugin machine back to that snapshot |
| 4 | Destroy a machine this plugin created |
| 5 | Agents: live status, and stop |
| r | Refresh, and sync the tree into Herdr |
| q | Close the menu |

Start asks for a name (blank lets boxd pick one), a size (Enter gives 2 vCPU / 8 GB), and whether to copy the focused pane's project. Then it shows a summary. Nothing is created until you confirm.

- **Copy** is git-aware. In a repo it sends tracked files, plus untracked files that aren't ignored, plus `.git`. Outside a repo it skips `target`, `node_modules`, `.venv`, and similar directories. It asks first if the upload is over 500 MB. It never copies `/` or your home directory. The shell opens in that directory, not `/home/boxd`.
- **Size.** A snapshot restore keeps the snapshot's size. Any other size costs one quick reboot.
- **Hostname.** If the size already matches, there is no reboot. The plugin restarts the machine's Herdr server instead, so shells that were restored from the snapshot pick up the new hostname.
- **If anything fails, or you close the popup or press Ctrl-C** after the machine exists, the plugin destroys it again. You never pay for a half-set-up machine.

When it finishes, the machine is in the Herdr sidebar.

## From the shell

```bash
herdr-boxd start --name my-box --size 2x8 --cwd "$PWD" --copy
herdr-boxd list                      # a tree: status, size, agents; flags machines deleted elsewhere
herdr-boxd list --orphans            # account machines that no reachable ledger tracks
herdr-boxd attach my-box             # wake a sleeping machine and reconnect it to Herdr
herdr-boxd sync                      # every running machine in the tree into Herdr here
herdr-boxd update my-box --yes       # overwrite the snapshot with this machine
herdr-boxd destroy my-box --yes      # and the machines it created
herdr-boxd config show
herdr-boxd config set snapshot herd-base
herdr-boxd config set api-key        # prompts without echo, or reads stdin
herdr-boxd config unset api-key
```

`--size` accepts `1x4`, `2x8`, `4x16`, or `8x32`, within your boxd quota. Outside a terminal, `start` uses the boxd default name and 2x8, and it copies only with `--copy`. Destructive commands need `--yes` when nobody is there to answer the question.

## Subagents

Agents call the CLI with `--json`. `herdr-boxd --skill` prints a skill file that teaches them how. Install it with `herdr-boxd --skill > ~/.claude/skills/herdr-boxd/SKILL.md`, or point your agent at `skills/herdr-boxd/SKILL.md`.

```bash
# New machine, copy this project, start Claude in a Herdr pane, hand it a task
herdr-boxd --json agent spawn --name fix-auth --cwd "$PWD" --prompt "Fix the auth tests"

# Fire-and-forget: claude -p / codex exec, detached, with logs
herdr-boxd --json agent spawn --name bench --headless --cwd "$PWD" --prompt-file task.md

herdr-boxd --json agent list
herdr-boxd --json agent wait fix-auth --timeout 600000
herdr-boxd --json agent read fix-auth
herdr-boxd --json agent prompt fix-auth "Now update the changelog" --wait
herdr-boxd --json agent stop fix-auth --destroy
```

- **Pane mode (default).** The agent runs in a Herdr workspace on the machine, so you can watch it or take over from the sidebar. Herdr reports `idle`, `working`, `blocked`, or `done`. Any Herdr agent kind works. For Claude, the plugin pre-accepts the "trust this folder" dialog for the work directory, because that directory is your own copied project or one the caller named.
- **Headless mode** (`--headless`, `claude` or `codex`). The agent runs detached in the machine, with logs in `~/.herdr-boxd/agents/<name>/` there. `read` returns a readable transcript and the final `result`. `prompt` resumes the same session. Headless agents run with permission checks bypassed: there is nobody to approve them, and they are inside your private VM.
- Herdr is only needed for pane mode. Without it (another terminal, CI), `--headless` works over the boxd API alone, and new machines just aren't added to a sidebar.
- Several agents can share a machine with `--machine NAME`. Parallel spawns are safe.
- A spawn that fails cleans up the machine it created. `stop --destroy` destroys the machine only if that agent created it and no other agent uses it.
- `status` and `list` never wake a sleeping machine. `wait`, `read`, and `prompt` do.

With `--json`, errors are `{"error":{"code","message"}}` on stderr. The exit code is 1 for errors and 2 for usage errors.

## Remote master

A master is a boxd VM that runs Herdr and an orchestrating agent, and creates machines with herdr-boxd. You can leave it running and connect to it from anywhere.

Set one up from your computer. The plugin goes in for the menu, and `install.sh` puts the CLI on the agent's `PATH`:

```bash
herdr-boxd start --name master --size 4x16      # or any boxd VM with Herdr
ssh -t master.boxd 'herdr plugin install SkymanOne/herdr-boxd --ref v0.3.0'
boxd machine exec master -- sh -c 'curl -fsSL https://raw.githubusercontent.com/SkymanOne/herdr-boxd/main/install.sh | sh'
```

Inside the master, auth is automatic. There is no boxd daemon and no SSH key there, so the first `start`, `attach`, or pane spawn creates `~/.ssh/herdr-boxd_ed25519`. Over the boxd exec stream it puts that key on the child, starts a key-only sshd there, and pins the child's host key. `ssh <child>.boxd` then works over the private network, so you never link a key by hand.

There are two ways to connect:

- **From Herdr on your Mac.** The master is in your sidebar if you started it with herdr-boxd. Run `herdr-boxd sync`, or press `r` in the menu, and the master's machines join your sidebar too.
- **Over SSH.** `ssh -t master.boxd herdr` opens the master's own Herdr, with its machines in its sidebar.

Each machine is recorded by the host that created it. `herdr-boxd list` on your computer shows the whole tree, three levels deep:

```
● master        running     4x16
● └ keen-otter  running     2x8 · 1 agent
```

`destroy master` lists every machine below it and asks once (or takes `--yes`). Then it destroys them deepest first, and the master last, so nothing is left billing untracked. If a machine below is asleep, destroy wakes it to look for machines of its own. If one cannot be checked, nothing is destroyed. `list --orphans` is read-only: it shows machines on the account that no ledger in the reachable tree tracks, such as leftovers from a master destroyed with `boxd` directly.

## Configuration and auth

The snapshot name defaults to `herd-base`. Auth is the first of: a stored API key, `BOXD_TOKEN`, `BOXD_API_KEY`, `boxd auth login`, or a token minted automatically inside a boxd VM. The key goes to the token exchange over stdin, so it never appears in `ps`. `config show` reports which source was used.

It only manages machines it created, and it updates the snapshot only when asked.

SSH: boxd's own daemon keeps `Host <name>.boxd` entries in `~/.ssh/config`. The plugin writes a fallback block only if the daemon hasn't written one, or inside a boxd VM, where there is no daemon (see [Remote master](#remote-master)). It writes atomically, preserves the file mode, and follows a symlinked config. Destroy removes the block again.

## License

[MIT](LICENSE)

