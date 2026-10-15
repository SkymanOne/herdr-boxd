# herdr-boxd

Start a [boxd](https://boxd.sh) machine from a snapshot and attach it in [Herdr](https://herdr.dev). Other agents can also use it to run coding subagents inside boxd machines and keep track of them.

It runs on macOS and Linux. You need Herdr 0.9.1 or newer, a `boxd` login on this computer, and Rust to build from source.

## Install

From this directory:

```bash
cargo build --release --locked
herdr plugin link .
ln -sf "$PWD/target/release/herdr-boxd" ~/.local/bin/herdr-boxd   # so agents can call it
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
| r | Refresh |
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
herdr-boxd list                      # status, size, agents; flags machines deleted elsewhere
herdr-boxd attach my-box             # wake a sleeping machine and reconnect it to Herdr
herdr-boxd update my-box --yes       # overwrite the snapshot with this machine
herdr-boxd destroy my-box --yes
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

## Configuration and auth

The snapshot name defaults to `herd-base`. Auth uses the token from `boxd auth login`. If that token is rejected, set an API key. The config file is written atomically with mode `600`. The key goes to the token exchange over stdin, so it never appears in `ps`.

SSH: boxd's own daemon keeps `Host <name>.boxd` entries in `~/.ssh/config`. The plugin writes a fallback block only if the daemon hasn't written one. It writes atomically, preserves the file mode, and follows a symlinked config. Destroy removes the block again.

