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
vm run linux --elevated -- apt-get upgrade -y          # the guest itself: no repo, no sync
vm ls
```

## Model

- **The host working tree is the single source of truth.** Before every exec,
  the dirty working tree (staging area untouched) is snapshotted as a git
  commit object and pushed over ssh to a per-guest native checkout under the
  guest's `work_root`. The guest resets to it and the tree hash is verified —
  guests always run exactly what *git* sees on the host's disk: uncommitted and
  untracked files included, gitignored files excluded. A gitignored `.env` or
  local fixture the build needs does not travel; a tracked file that also
  matches `.gitignore` does.
- **`--with-file` is the way past that**, for the `.env` the build cannot start
  without: `vm exec lin --with-file .env -- cargo test` forces the file into the
  same snapshot, so it arrives tree-hash-verified like everything else. It lives
  in the guest exactly as long as the flag does — the next sync without it takes
  the file back out. Its contents do land on the guest's disk, so for a value
  that must not, `-e NAME=value` (or bare `-e NAME` to forward the host's) passes
  it to the process and nowhere else. When a guest command fails and the repo has
  a gitignored `.env` that stayed behind, vm says so rather than letting you hunt
  for it.
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
VM a short boot away from being up. So every command that needs a guest (`exec`,
`sync`, `claude`, `deploy`, `clean`, `shot`, and `doctor <alias>`) starts it,
and says so:

```
vm ▸ linux ▸ 'Ubuntu 24.04' is stopped — starting it…
vm ▸ linux ▸ ready at 10.211.55.4 ▸ 10.5s
```

A VM that is already running prints neither line — silence means there was
nothing to wait for. A long wait is narrated as it goes (`vm ▸ macos ▸ 'macOS'
running, no IP yet — 10s of 90s`), and a VM that is not coming up at all fails
in ~15s rather than sitting out the full timeout.

The other half is `vm reap`, which **shuts down** VMs that no vm process is
using and that have been idle a while (30m by default; `vm reap --install` runs
it every 5 minutes from launchd). It leaves a VM alone while someone is typing
at its console, so a guest you are driving by hand in the Parallels GUI is not
pulled out from under you. Nobody is watching a launchd job, so every sweep's
decision — kept, skipped, shut down, and why — goes to [the
journal](#the-journal) with the time it was made. If you upgraded from a vm that
predates the journal, `vm doctor` will tell you to re-run `vm reap --install`:
the old job keeps writing to a log nothing rotates until you do.

Between the two, VM lifecycle is never a step anyone has to remember — which is
why the verbs for it do not exist. A graceful stop is the only way vm puts a VM
down. It shuts down rather than suspends: a suspended guest's saved state can
turn out to be one Parallels itself refuses to restore, which strands the VM
entirely, while a boot is only seconds slower and always works.

For that graceful stop to stay graceful on a linux guest, `vm deploy` installs a
systemd unit (`vm-prldnd-shutdown.service`). Parallels' drag-and-drop agent
ignores the SIGTERM it is sent at shutdown and jams gnome-session's logout, so
the Ubuntu guest took 92–99s to stop — and Parallels force-kills a guest that
takes 120s. The unit SIGKILLs that agent as the shutdown opens; stops now take
~4s. `vm doctor` fails if it is missing, disabled, inactive, or out of date,
because nothing else would notice a guest drifting back to twenty seconds from a
force-kill.

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

### Running a script

**Write the script to a file in the repo and run it.** The sync carries
uncommitted *and untracked* files, so a script you just created is already in the
guest — no copying, no staging, no commit:

```sh
cat > check.py <<'EOF'
import platform; print("hello from", platform.system())
EOF
vm exec linux -- python3 check.py     # untracked; the sync takes it anyway
```

This is the path to reach for. The file is versioned with everything else, it is
there for the next run, and it has no size limit.

Two things to know about the alternatives:

- **A heredoc works**, because one argument is a script for the guest's shell:
  `vm exec linux -- 'python3 - <<EOF … EOF'`. Convenient for a throwaway. But on
  Windows, `cmd.exe` refuses a command line over **8191 characters**, so a long
  inline script must be a file.
- **Piping the script into vm does not work.** `vm exec` never forwards its own
  stdin (see [Stdio](#stdio)) — `cat script.py | vm exec linux -- python3` runs
  python with *no input* and exits 0, having done nothing. If you want to feed a
  command on stdin, that is what [`vm run`](#ad-hoc-commands-vm-run) does.
- **A gitignored script does not travel.** Name it with `--with-file`, or keep
  scripts out of ignored paths.

## Ad-hoc commands (`vm run`)

`vm exec` is for *this repo's code*: it finds the repo, syncs it, and runs in the
checkout. When the **guest itself** is the subject — patch it, install a tool,
ask what version of something it has — that is `vm run`, which has no repo and no
sync behind it and therefore needs neither:

```sh
vm run linux -- uname -a                         # runs in the guest user's home
vm run windows --elevated -- winget upgrade --all
vm run linux --elevated -- apt-get update
vm run macos --elevated -- sh < maintenance.sh   # a script, on stdin
```

It works from anywhere — no git repo required — and takes the same command forms
as exec (several arguments are an argv; one argument is a script for the guest's
shell). It holds the same use-lock as exec, so `vm reap` will not shut a guest
down out from under a long `apt-get upgrade`.

**`--elevated`** runs as **root** (linux/macos) or **SYSTEM** (windows) through
Parallels Tools. It is the only elevation there is: `sudo` over ssh wants a
password, and the Windows guest user is not an administrator (UAC cannot be
satisfied headless). It needs no console login. Note that the superuser's `PATH`
is the *system* one — per-user tools (`mise`, `cargo`, a user-scope `brew`) are
not on it, which is correct for `apt`/`winget`/`softwareupdate` and wrong for
anything installed under the config user's home. Run those without `--elevated`.

**Stdin travels — this is the one command where it does.** Input piped or
redirected into `vm run` becomes the guest command's stdin (up to 8 MiB of text),
which is how a script gets in:

```sh
vm run linux --elevated -- sh < step.sh          # exit code is the script's own
```

That is also the *only* way to send a large script through the elevated
transport: `prlctl exec` hangs forever — silently, and immune to SIGTERM — once
its **total** command line passes roughly 4 KB (many small arguments hang it just
as reliably as one big one). vm refuses to build such a command line rather than
let it hang; on stdin there is no such limit, because the payload rides inside
the request the agent reads.

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
- `--writeback` / `--no-sync` / `--with-file` compose but are no-ops on the
  native path (nothing syncs, and the file is already where it lives); the guest
  env's wrap prefix (below) is **not** applied natively — the launching
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

On `127`, vm also prints the PATH the command was searched on — which is never
the PATH of the shell you are standing in: the agent augments the guest's,
`--or-native` gets whatever the runner exported, and `-e PATH=…` overrides
either. On Windows it flags entries left in POSIX form (`/c/tools`, or a whole
colon-joined run of them), which no Win32 resolver can search and which a `mise`
task with `shell = "bash -c"` hands its native children.

`vm doctor` is the one exception: it reports **1** when any check fails (it has
no guest command whose status it could be confused with).

Two ambiguities are unavoidable and shared with `ssh`/`docker`: `255` can be an
ssh connection failure *or* a guest command that itself exited 255, and `127`
can be a missing guest agent *or* a genuine command-not-found. The `vm ▸ …`
breadcrumb and the `vm: error:` / `vm: config error:` line disambiguate. Because
255 may mean the connection dropped mid-run, `--writeback` **skips** the
writeback on that code rather than trusting the guest tree — and says so.

## Stdio

A guest command's stdout and stderr stream through untouched, and every line vm
itself prints — breadcrumbs, notes, errors — goes to **stderr**. Stdout is
therefore exactly the command's own: `vm exec linux -- 'cargo test 2>&1' >
log.txt` captures a clean log, and piping vm's stdout into another tool never
picks up vm chatter.

**`-q` silences the narration, never the news.** Breadcrumbs (`vm ▸ linux ▸ exit
0 ▸ 3.2s`) disappear; notes, `WARNING:` lines and errors still print, because a
quiet flag that swallowed the reason a run failed would be a trap. It has to come
before the `--` — after it, the guest command owns the argv.

Stdin is the deliberate exception: **`vm exec` never forwards it.** vm keeps the
host↔guest pipe for itself as the liveness channel — it sends the request, then
a keepalive every 15s for as long as the command runs — and the guest command
reads from the null device. A killed `vm` (Ctrl-C, `kill -9`) stops beating, and
the agent takes that as its cue to tear the guest process tree down rather than
leave a compiler running in there. Over ssh the pipe *closes* as well, so
teardown is immediate. Over `prlctl` (`vm exec windows`, `vm run --elevated`) it
may not close at all — Parallels Tools can leave the guest's end of stdin open
long after the host is gone, which is how a killed `vm run --elevated macos`
used to leave its command running forever — so there the silence is the news,
and the tree comes down within the minute instead.

**A killed `vm run --elevated macos` leaves a wake**, and it is worth knowing the
shape of it. Parallels frees the host-side session immediately but leaves the
guest's stdin open, so the orphaned agent lives out its silence budget and only
then tears down — and a minute after *that*, Parallels fires a delayed cleanup
retry for the session it already forgot. The retry closes the stdin of whichever
session now holds the slot. A run started in the ~60s between those two events
therefore dies with an **exit 130** it never earned, up to a minute in, while
behaving perfectly. Re-run it: a run started before the teardown or after the
retry is safe. It is a Parallels bug on the macOS guest (the Windows console
channel is unaffected), and not one vm can absorb — once that pipe is closed no
more keepalives arrive, so the silence budget would end the run anyway. To tell
it from a real Ctrl-C, take the death's timestamp from [the
journal](#the-journal) — `grep 'exit 130' ~/.config/vm/log/vm.log` — and look for
a `Failed to find guest exec session` line in `~/Parallels/macOS.macvm/parallels.log`
within milliseconds of it; an interrupt you actually sent leaves none. (This is
the correlation the journal was added for: before it, vm recorded no time of
death, so there was nothing to line up against.) See
[#27](https://github.com/akesson/vm/issues/27).

So `echo hi | vm exec linux -- 'cat > f'` writes an empty file and
exits 0 — vm prints a `vm ▸ note:` when it sees input wired into its stdin, so
that near-miss is never silent. To feed an exec'd command data, put it in a file
in the repo: the sync carries it, verified like everything else.

**`vm run` is the exception to the exception**: there, input piped or redirected
into vm *is* the guest command's stdin (≤ 8 MiB of text), and vm says so — `vm ▸
linux ▸ stdin ▸ 91 bytes → the guest command`. It rides inside the request rather
than down the pipe, so the liveness channel is untouched. Binary input is refused
rather than mangled. See [Ad-hoc commands](#ad-hoc-commands-vm-run).

## The journal

Every line vm prints to stderr it also *keeps*, in `~/.config/vm/log/vm.log`,
stamped with the local time and the pid that wrote it:

```
2026-07-14T16:19:48.412+02:00 [8831] vm ▸ macos (macOS) ▸ ~/work/vm ▸ $ cargo test
2026-07-14T16:19:58.905+02:00 [8831] vm ▸ macos ▸ 'macOS' is stopped — starting it…
2026-07-14T16:20:36.107+02:00 [8831] vm ▸ macos ▸ exit 130 ▸ 47.2s
2026-07-14T16:24:48.001+02:00 [9014] vm ▸ reap ▸ linux idle 12m of 30m — kept
```

It is a transcript, not an event stream: the line in the file is the line you
saw, so it cannot drift out of sync with what vm actually said. The pid ties one
invocation's lines together — concurrent `vm` runs against one guest are normal,
and they share the file. `-q` does not silence it; a quiet run is still a run you
can read back.

It exists because the unattended half of vm had no memory. `vm reap` decides
every five minutes whether to shut a VM down, and those decisions used to go to a
file launchd redirected on vm's behalf — with **no timestamps**, so the one file
you would open to ask *why did my VM go down at 3pm* could not tell you when
anything happened, and no rotation, so it grew forever. vm now writes its own,
rotating at 8 MiB and keeping one generation behind it (a 16 MiB ceiling).
`vm doctor` reports where it is and how big it has got.

Two things worth knowing. The journal keeps the **command lines you ran**, which
were previously ephemeral on a terminal — hence mode `0600`, and `VM_JOURNAL=off`
to opt out of a file entirely. And it keeps *vm's* lines: a guest command's own
output streams through to your terminal untouched, as it always has, and is not
captured. `vm exec linux -- 'cargo test' > log.txt` is still how you keep that.

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
"ubi:akesson/vm" = "0.2.0"   # pin it: CI stays reproducible, and a release is one git tag
```

A `[tools]` entry shadows any `vm` already on PATH — including a
`cargo install` dev build — wherever mise is active for that repo. If the dev
machine should keep its own build (say, because you hack on vm itself), put
the entry in `mise.ci.toml` and set `MISE_ENV=ci` on the runner instead: CI
installs the pinned release, dev machines never see it.

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
