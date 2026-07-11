# vm

Run commands in Parallels VMs against a synced copy of the current repo — one
tool, installed on the host **and** in every guest.

```sh
vm exec windows -- cargo nextest run -p my-windows-crate
vm exec windows -e RUST_BACKTRACE=1 -- cargo nextest run -p my-windows-crate
vm exec linux -- 'cd src && cargo clippy | head -20'   # one argument = a shell script
vm exec linux --writeback -- cargo clippy --fix
vm exec linux --with-snapshot -- ./install-something-destructive.sh
vm exec --or-native windows -- cargo nextest run  # native when the host is already Windows
vm claude windows "fix the test that only fails on Windows"
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
- **VMs take care of themselves.** There is no `vm start` and no `vm stop`: see
  below.

## VM lifecycle

A VM being down is not a state you have to fix before you can use it — it is a
VM one second away from being up. So every command that needs a guest (`exec`,
`sync`, `claude`, `deploy`, `clean`, `shot`, and `doctor <alias>`) starts or
resumes it, and says so:

```
vm ▸ linux ▸ 'Ubuntu 24.04' is suspended — resuming it…
vm ▸ linux ▸ ready at 10.211.55.4 ▸ 3.0s
```

A VM that is already running prints neither line — silence means there was
nothing to wait for. A long wait is narrated as it goes (`vm ▸ macos ▸ 'macOS'
running, no IP yet — 10s of 90s`), and a VM that is not coming up at all fails
in ~15s rather than sitting out the full timeout.

The other half is `vm reap`, which **suspends** VMs that no vm process is using
and that have been idle a while (30m by default; `vm reap --install` runs it
every 5 minutes from launchd). It leaves a VM alone while someone is typing at
its console, so a guest you are driving by hand in the Parallels GUI is not
pulled out from under you.

Between the two, VM lifecycle is never a step anyone has to remember — which is
why the verbs for it do not exist. Suspending is the only way vm puts a VM down;
a full power-off is a Parallels operation (`prlctl stop`).

## Targets

A target is always a **VM alias** — a `[vm.<alias>]` key in the machine config.
There is no second addressing scheme: `vm exec`, `vm sync`, `vm doctor`, and
every other command take the same alias, and an unknown one is an exit-2 config
error listing what is configured.

**Name each alias after its OS** (`windows`, `linux`, `macos`) unless you have a
reason not to — that is what makes `--or-native` task lines portable to CI (see
below), and it keeps one name for one machine.

## Command forms

How a command is written decides how it runs — the same split a Dockerfile's
`RUN` makes, and the same one `docker exec` leaves you to make by hand:

| What you pass after `--` | What runs | For |
|---|---|---|
| **several arguments** | exec'd exactly as given, no shell anywhere | everything normal: `vm exec linux -- cargo test --workspace` |
| **exactly one argument** | run as a script by the guest's shell (`sh -c`, or `cmd /C` on Windows) | pipes, `&&`, redirects, `cd` and other builtins: `vm exec linux -- 'cd src && cargo test'` |

Arguments in the first form reach the guest **byte-identical** — they travel as
JSON to a guest agent, with no shell quoting layer anywhere between your
terminal and the process. So `vm exec linux -- grep 'a|b' src/lib.rs` keeps its
regex: the `|` is data, not a pipe.

The rule counts arguments; it never reads them. That is deliberate — the `|` in
`grep 'a|b' f` and the one in `echo hi | wc` are the same byte with opposite
meanings, so no amount of inspecting the command could tell them apart. Instead
vm prints a `vm ▸ note:` when the form you used looks unlikely to be the one you
meant (a lone `&&` sitting in an argv, say). The note is advice on stderr; it
never changes what runs. And the `$ …` breadcrumb always shows the **literal**
command the guest gets, wrap and shell included, so what ran is never a guess:

```
vm ▸ linux (Ubuntu 24.04) ▸ ~/work/vm ▸ $ mise exec -- sh -c 'cd src && cargo test'
```

One edge worth knowing: a single argument is a *script*, so a lone path whose
filename contains a space gets word-split by the shell like any other script
would. Quote it for the shell — `vm exec macos -- '"/Applications/My App/run"'` —
or just pass it in exec form. vm notes this one for you when it sees it.

## Native or VM (`--or-native`)

By default `vm exec` **always** runs in a VM, even when the target's os is the
host's own — so where a command runs is never ambiguous. `--or-native` opts into
one exception: if the host OS already matches the target's os, the command runs
**natively** (no VM, no sync) with a loud `vm ▸ native (…)` banner instead.

This lets a single task-runner entry drive a guest on a dev host and run in
place on a CI runner that is already the target OS — where there is no Parallels
and no machine config:

```toml
# mise.toml — same line on a Mac dev host (→ Windows guest) and on windows-latest (→ native)
[tasks."win:test"]
run = "vm exec --or-native windows -- cargo nextest run -p my-windows-crate"
```

For that to work on the runner, the target must be **literally** `windows`,
`linux`, or `macos`: an os-named target is matched against the host *before* the
machine config is loaded, so it goes native on a machine that has neither config
nor Parallels. On the dev host the same name is looked up as an ordinary alias —
which is why the VM's alias should be its OS name. (`vm exec --or-native windows`
against a config whose Windows VM is called `win` is an error, and says so.)

- Omit the flag to **force the VM** even on a matching host — e.g. a macOS host
  driving the macOS guest for UI tests (the whole reason that guest exists).
- `--writeback` / `--no-sync` compose but are no-ops on the native path; the
  guest env's wrap prefix (below) is **not** applied natively — the launching
  environment already is the environment. `--or-native` cannot be combined with
  `--with-snapshot` (the host cannot be snapshotted).

## Guest environments (`--guest-env`)

A guest checkout is a fresh copy of the repo on another OS, so a repo whose
tools are managed by a dev-environment tool needs that tool set up there once,
and its commands run under it. `vm` handles this for **mise**:

| | |
|---|---|
| detected by | `mise.toml` / `.mise.toml` / `.config/mise/config.toml` … at the repo root |
| on first sync | runs `mise trust` in the guest checkout (once per checkout creation) |
| on every exec | runs the command as `mise exec -- <cmd>`, so the repo's tools resolve |

With a script (one argument), the wrap goes *around* the shell — `mise exec --
sh -c '<script>'` — so the whole script runs inside the environment, builtins
and pipelines included, and its exit code comes back as its own.

**The first wrapped exec in a fresh guest can take minutes.** `mise exec`
installs whatever the repo's `[tools]` block asks for before it runs anything —
so even `vm exec windows -- echo hi` will sit there building the toolchain the
first time, then take ~1s on every later run once the guest has it cached. That
is mise doing its job, not vm hanging. `--guest-env none` skips it.

Detection is **never silent** — an active guest env announces itself before it
does anything:

```
vm ▸ windows ▸ guest env: mise (detected mise.toml) — `mise trust` on first sync,
     exec commands wrapped `mise exec --`; --guest-env none disables
```

Override per invocation with `--guest-env mise` (force it without a marker file)
or `--guest-env none` (run the bare command, no setup, no wrap). `vm claude` is
wrapped too, so the commands Claude runs inside the guest see the repo's tools.
This replaces the old `.vm.toml` (`on_first_sync` / `wrap`), which no longer
exists.

## Exit codes

`vm` passes a guest command's exit code straight through, and reserves two codes
for its *own* failures so a caller — a shell, a `mise` fan-out — can tell "the
command failed" from "vm failed" and retry only the latter:

| Code | Meaning |
|---|---|
| `0` | the guest command succeeded |
| `2` | **vm usage/config error** — bad invocation, unreadable or invalid config, unknown alias, run outside a git repo. Fix your setup; retrying won't help. (Also clap's own argument-parse errors.) |
| `125` | **vm infrastructure error** — sync, agent, ssh/transport, or VM lifecycle. vm itself failed and the command may not have run; often transient, so a retry may help. |
| `126` / `127` | guest command found-but-not-executable / not found |
| other | the guest command's own exit code, untouched (signal death shows as `128 + signal`) |

`vm doctor` is the one exception: it reports **1** when any check fails (it has
no guest command whose status it could be confused with).

Two ambiguities are unavoidable and shared with `ssh`/`docker`: `255` can be an
ssh connection failure *or* a guest command that itself exited 255, and `127`
can be a missing guest agent *or* a genuine command-not-found. The `vm ▸ …`
breadcrumb and the `vm: error:` / `vm: config error:` line disambiguate. Because
255 may mean the connection dropped mid-run, `--writeback` **skips** the
writeback on that code rather than trusting the guest tree — and says so.

## Claude in a VM

`vm claude <alias> "<prompt>"` runs Claude Code headless (`claude -p`) in
the guest checkout. The VM is the permission boundary, so Claude runs with
`--dangerously-skip-permissions` — it can do anything inside the guest, but
the host tree only ever receives the writeback diff (on by default; opt out
with `--no-writeback`). Add `--with-snapshot` to roll the guest itself back
afterwards, so nothing survives the run but the diff. `-e NAME=value` /
`-e NAME` forward env vars to the guest claude process.

vm's own flags must come **before** the prompt; everything after it goes to
`claude` verbatim (e.g. `--model sonnet`). A vm flag that lands in that tail is
rejected (exit 2) rather than silently handed to claude — `--no-writeback` is
what keeps vm out of your host tree, so quietly dropping it is not an option.

Requires the `claude` CLI installed and logged in inside the guest —
`vm doctor` checks both.

## Install

Prebuilt binaries are published per release with GitHub-triple asset names, so
they install via `mise`'s `ubi` backend (or `ubi` directly). A `mise` tools
block makes `vm` available for free (and cached) on both dev machines and CI
runners:

```toml
# mise.toml
[tools]
"ubi:akesson/vm" = "latest"   # or pin a version, e.g. "0.1.0"
```

Or from source: `cargo install --path .` (or `mise run install`).

Releases cover arm64 macOS/Linux/Windows and x86_64 Linux/Windows. Intel macOS
(x86_64) is not published — install from source there. Deploy the matching agent
into each guest with `vm deploy <alias>`.

## Setup

Host config lives at `~/.config/vm/config.toml` (override the path with
`$VM_CONFIG` — handy for CI and tests):

```toml
[vm.windows]                        # the alias: how every command names this VM
parallels_name = "Windows 11"       # exact name from `prlctl list -a`
os = "windows"                      # windows | linux | macos
user = "henrik"                     # guest user for ssh
work_root = 'C:\work'               # guest dir holding per-repo checkouts
# host = "10.0.0.5"                 # optional; else the IP comes from prlctl
# agent_path = '…'                  # optional; else <home>/.vm/bin/vm[.exe]

[vm.linux]
parallels_name = "Ubuntu 24.04"
os = "linux"
user = "parallels"
work_root = "~/work"
```

That is the whole configuration surface — there is no per-repo config file. A
repo's guest setup is derived from the repo itself (see **Guest environments**).

`vm doctor` checks host and guests; `vm deploy <alias>` builds and installs
the agent inside a guest.

## Issues

Bugs and rough edges go to
[github.com/akesson/vm/issues](https://github.com/akesson/vm/issues). Claude
sessions driving `vm` are encouraged to do the same: when the tool itself
misbehaves — not the project it's running against — file it with
`gh issue create --repo akesson/vm`, including the failing `vm ▸ …` breadcrumb
and the guest OS. Check for an existing report first.
