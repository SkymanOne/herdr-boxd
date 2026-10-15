---
name: herdr-boxd
description: "Spawn and monitor coding subagents (Claude Code, Codex, and other Herdr-supported agents) inside isolated boxd cloud machines with the `herdr-boxd` CLI. Use when a task should run in a sandboxed cloud VM, when fanning work out to parallel subagents, or when the user mentions herdr-boxd, boxd subagents, or delegating to a boxd machine."
---

# herdr-boxd: subagents in boxd machines

`herdr-boxd` creates boxd machines from a snapshot, connects them to Herdr, and
runs coding agents inside them. Every command takes `--json`; use it always.

- Results: JSON on stdout.
- Errors: `{"error":{"code":"...","message":"..."}}` on stderr, exit 1. Usage errors exit 2.
- Progress lines go to stderr; ignore them or show them to the user.

Check it is installed with `herdr-boxd --version`. Auth is a stored API key,
`BOXD_TOKEN`, `BOXD_API_KEY`, `boxd auth login`, or automatic inside a boxd VM.
The plugin and pane mode need Herdr 0.9.1 or newer on this
computer; headless mode works without Herdr, from any shell or CI.

## Two modes

| | pane (default) | `--headless` |
| --- | --- | --- |
| Runs as | interactive agent in a Herdr pane on the machine | `claude -p` / `codex exec`, detached |
| User sees it | yes, in the Herdr sidebar | no, logs only |
| States | `idle` `working` `blocked` `done` `unknown` `exited` | `working` `done` `failed` `lost` |
| Without `--wait` | prompt reports `submitted` | spawn reports `working` |
| Follow-ups | `agent prompt` any time it is idle | `agent prompt` resumes the same session |
| Kinds | any Herdr agent kind (`claude`, `codex`, `gemini`, ...) | `claude`, `codex` |
| Permissions | the agent asks; you answer with `agent keys` | runs with permission checks bypassed, inside the VM |

Prefer pane mode for work the user may want to watch or take over. Use headless
for fire-and-forget batch work.

Either mode can also report `machine_<status>` (for example
`machine_hibernated`), `machine_gone`, or `unreachable`. Status checks never
wake a machine. `wait`, `read`, and `prompt` wake it first.

## Spawn

```bash
# New machine, copy the current project (git-aware), start Claude, give it a task:
herdr-boxd --json agent spawn --name fix-auth --cwd "$PWD" \
  --prompt "Fix the failing auth tests. Commit when green." 

# Several in parallel, each on its own machine (run these concurrently):
herdr-boxd --json agent spawn --name api   --cwd "$PWD" --prompt-file tasks/api.md &
herdr-boxd --json agent spawn --name ui    --cwd "$PWD" --prompt-file tasks/ui.md &
wait

# Reuse a machine this plugin created (several agents can share one):
herdr-boxd --json agent spawn --name reviewer --machine hb-12 --workdir /home/boxd/app \
  --prompt "Review the last commit."

# Headless, blocking until it finishes (timeout in ms):
herdr-boxd --json agent spawn --name bench --headless --cwd "$PWD" \
  --prompt "Run the benchmarks and summarise regressions." --wait --timeout 1800000

# Native agent arguments go after --:
herdr-boxd --json agent spawn --kind codex --prompt "..." -- --model gpt-5
```

Options: `--name` (`[a-z][a-z0-9_-]{0,31}`, default `<kind>-xxxx`), `--kind`
(default `claude`), `--machine M` or `--size 1x4|2x8|4x16|8x32` for a new
machine (default 2x8), `--cwd PATH` copies a local project to
`/home/boxd/<dirname>` and works there, `--workdir DIR` works in an existing
remote directory instead, `--prompt TEXT`, `--prompt-file FILE` (`-` reads
stdin), `--wait`, `--timeout MS`.

The reply looks like:

```json
{"agent":"fix-auth","machine":"keen-otter","kind":"claude","mode":"pane",
 "status":"working","workdir":"/home/boxd/myrepo","workspace":"w3"}
```

A spawn that fails before the agent starts cleans up after itself, including
the machine it created. Once the agent is running it is never torn down: if
`--wait` times out or the command is interrupted, the error says the agent
keeps running and `agent status` picks it up.
A pane spawn that reports `"status":"blocked"` started but is stuck on a
dialog: `agent read` it, then answer with `agent keys`, or ask the user.

## Monitor

```bash
herdr-boxd --json agent list                    # every agent, with live status (also those of machines you created)
herdr-boxd --json agent status fix-auth
herdr-boxd --json agent wait fix-auth --timeout 600000   # until idle/done/blocked (pane) or finished (headless)
herdr-boxd --json agent read fix-auth --lines 200        # screen text (pane) or log + "result" (headless)
```

`wait` exits 1 with code `timeout` if the deadline passes; the agent keeps
running. A timeout does not mean the work failed: read before deciding.

For headless agents `read` returns `text` (a readable transcript), `result`
(the final answer once done), `stderr`, and the raw `events`.

## Steer

```bash
herdr-boxd --json agent prompt fix-auth "Also update the changelog." --wait --timeout 600000
herdr-boxd --json agent keys fix-auth down enter      # pane only: answer a dialog
herdr-boxd --json agent keys fix-auth ctrl+c          # pane only: interrupt
```

`prompt` on a headless agent that is still `working` fails with `busy`.

## Finish

```bash
herdr-boxd --json agent stop fix-auth             # stop the agent, keep the machine
herdr-boxd --json agent stop fix-auth --destroy --yes  # also destroy the machine it created, and its machines
```

Get results out before destroying: have the agent `git push`, or `agent read`
its result. `--destroy` keeps a machine that other agents still use or that the
agent did not create. Machines left running cost money; clean up when done.

## Machines

```bash
herdr-boxd --json list                 # plugin machines as a tree: status, size, agents, children
herdr-boxd --json list --orphans       # account machines no reachable ledger tracks (read-only)
herdr-boxd --json attach M             # wake + reconnect to Herdr
herdr-boxd --json sync                 # add every running machine in the tree to Herdr here
herdr-boxd --json destroy M --yes      # destroys M's own machines first, deepest first
```

The plugin only manages machines it created. For anything else use the `boxd`
CLI directly.

A machine can run herdr-boxd itself (a "master"), so machines form a tree.
`list` nests each machine's own machines under `children`, three levels deep.
It never wakes a sleeping machine. A running machine whose machines could not
be read has `unreachable` set. In `agent list`, an agent that another
machine manages has `host` set. Use the usual commands on it anyway. `agent
status/wait/read/prompt/keys/stop`, `attach` and `destroy` run on the managing
host for you, with the same JSON and exit codes. Two agents with one name on
different hosts give `ambiguous`.
Inside a boxd VM, SSH to the machines it creates needs no setup; the plugin
handles the key.

## Error codes worth handling

`usage`, `auth` (API key, `BOXD_TOKEN`, `BOXD_API_KEY`, `boxd auth login`, or a boxd VM), `quota`,
`already_exists`, `not_found`, `not_owned`, `snapshot_not_found`, `busy`,
`timeout`, `agent_not_ready`, `unavailable` and `unreachable` (retry),
`ssh_unreachable`, `machine_gone`, `ambiguous`, `herdr_missing` (use `--headless`), `herdr_old` (install Herdr 0.9.1 or newer), `workdir_in_use` (another agent works in
that directory; share it with `--workdir` instead of copying over it),
`cancelled`.
