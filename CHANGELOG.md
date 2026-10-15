# Changelog

## 0.3.0 - 2026-10-15

A boxd VM can be a master: it runs Herdr and an agent, creates machines with herdr-boxd, and reaches them over SSH.

- SSH from a VM. Inside a boxd VM the plugin makes `~/.ssh/herdr-boxd_ed25519` once. Over the boxd exec stream it adds that key to the machine, starts a key-only sshd there, and pins its host key. `<name>.boxd` then reaches it on the private network. boxd's SSH gateway is not used from a VM, because linking a key needs an interactive login.
- One tree of machines. Ledger entries record the host that created them. `list` and `agent list` also run herdr-boxd inside each running machine, up to 3 levels deep. They never wake a sleeping machine, and they mark unreachable ones. JSON nests `children`. Agents from other hosts carry `host`.
- `sync` adds every running machine in the tree to Herdr here, and removes plugin profiles whose machine is gone. `start`, pane `agent spawn` on a new machine, and the menu's refresh run it too.
- `destroy` and `agent stop --destroy` also destroy the machines below, deepest first, after one question or `--yes`. If a machine below cannot be checked, nothing is destroyed.
- `list --orphans` shows machines on the account that no reachable ledger tracks.
- Fixed: `BOXD_VM_NAME` now wins over the stale lowercase `boxd_vm_name` that machines inherit from a snapshot.

## 0.2.0 - 2026-10-15

Inside a boxd VM, auth is automatic. `BOXD_TOKEN` and `BOXD_API_KEY` also work. The in-VM token is renewed before it expires.

## 0.1.0 - 2026-10-15

Initial release. The plugin and pane mode need Herdr 0.9.1. Headless works without Herdr. A copied project's shell opens in that directory.
