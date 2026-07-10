# vm

Run commands in Parallels VMs against a synced copy of the current repo — one
tool, installed on the host **and** in every guest.

```sh
vm exec windows -- cargo nextest run -p my-windows-crate
vm exec lin --writeback -- cargo clippy --fix
vm claude win "fix the test that only fails on Windows"
vm ls
```

## Model

- **The host working tree is the single source of truth.** Before every exec,
  the dirty working tree (staging area untouched) is snapshotted as a git
  commit object and pushed over ssh to a per-guest native checkout under the
  guest's `work_root`. The guest resets to it and the tree hash is verified —
  guests always run exactly what is on the host's disk.
- **Builds happen on guest-local disk.** No shared folders: no cross-platform
  `target/` conflicts, native file watching and locking, native speed.
- **One-way by default.** Guests cannot corrupt the host tree; `--writeback`
  explicitly returns source changes made in the guest (e.g. `clippy --fix`).
- **ssh is the transport, prlctl does what only it can**: VM lifecycle,
  IP discovery, screenshots, snapshots, and first-time bootstrap — plus
  Windows exec: `prlctl exec` carries the command into the console session
  (ssh children land in session 0, where UIA and other GUI APIs see an empty
  desktop), so GUI automation works on all three guests with plain `vm exec`.

## Exit codes

`vm` passes a guest command's exit code straight through, and reserves two codes
for its *own* failures so a caller — a shell, a `mise` fan-out — can tell "the
command failed" from "vm failed" and retry only the latter:

| Code | Meaning |
|---|---|
| `0` | the guest command succeeded |
| `2` | **vm usage/config error** — bad invocation, unreadable or invalid config, unknown alias/target/OS, run outside a git repo. Fix your setup; retrying won't help. (Also clap's own argument-parse errors.) |
| `125` | **vm infrastructure error** — sync, agent, ssh/transport, or VM lifecycle. vm itself failed and the command may not have run; often transient, so a retry may help. |
| `126` / `127` | guest command found-but-not-executable / not found |
| other | the guest command's own exit code, untouched (signal death shows as `128 + signal`) |

Two ambiguities are unavoidable and shared with `ssh`/`docker`: `255` can be an
ssh connection failure *or* a guest command that itself exited 255, and `127`
can be a missing guest agent *or* a genuine command-not-found. The `vm ▸ …`
breadcrumb and the `vm: error:` / `vm: config error:` line disambiguate.

## Claude in a VM

`vm claude <target> "<prompt>"` runs Claude Code headless (`claude -p`) in
the guest checkout. The VM is the permission boundary, so Claude runs with
`--dangerously-skip-permissions` — it can do anything inside the guest, but
the host tree only ever receives the writeback diff (on by default; opt out
with `--no-writeback`). Add `--with-snapshot` to roll the guest itself back
afterwards, so nothing survives the run but the diff. Extra flags after the
prompt go to claude verbatim (e.g. `--model sonnet`).

Requires the `claude` CLI installed and logged in inside the guest —
`vm doctor` checks both.

## Setup

Host config lives at `~/.config/vm/config.toml`:

```toml
[vm.win]
parallels_name = "Windows 11"
os = "windows"
user = "henrik"
work_root = 'C:\work'

[vm.lin]
parallels_name = "Ubuntu 24.04"
os = "linux"
user = "parallels"
work_root = "~/work"
```

`vm doctor` checks host and guests; `vm deploy <alias>` builds and installs
the agent inside a guest.

## Issues

Bugs and rough edges go to
[github.com/akesson/vm/issues](https://github.com/akesson/vm/issues). Claude
sessions driving `vm` are encouraged to do the same: when the tool itself
misbehaves — not the project it's running against — file it with
`gh issue create --repo akesson/vm`, including the failing `vm ▸ …` breadcrumb
and the guest OS. Check for an existing report first.
