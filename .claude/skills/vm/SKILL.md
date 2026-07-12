---
name: vm
description: Run commands in Parallels VMs (Windows/Linux/macOS) from any git repo. Use when a build/test/lint must run on another OS, or when the repo's scripts wrap `vm exec`. Handles code sync automatically — no shared folders, no manual copying.
---

# vm — cross-VM exec & sync

`vm` runs a command inside a Parallels guest **against a guest-local checkout
of the current repo**. Before every exec it snapshots the host working tree
(including uncommitted + untracked files, excluding gitignored ones) and syncs
it to the guest via git objects. You never copy files yourself.

A gitignored file the guest build needs — a `.env`, a local fixture — is the one
thing that does *not* travel by default. `--with-file .env` forces it into the
sync; `-e NAME=value` passes a value without ever writing it to guest disk. See
**Gitignored files** below.

## Mental model

- You always invoke `vm` on the host, from inside the repo you care about.
- A target is always a **VM alias** from `~/.config/vm/config.toml` — the same
  name for every command. `vm ls` lists them. There is no other addressing
  scheme; an unknown alias is an exit-2 config error that lists the real ones.
  (Aliases are usually named for their OS: `windows`, `linux`, `macos`.)
- The guest checkout lives at `<work_root>/<repo-name>` (e.g. `~/work/myrepo`).
  It is a *replica*: syncs `reset --hard` + `clean -fd` it. Never hand-edit it;
  gitignored files (build caches) survive syncs.
- Every run prints breadcrumbs to stderr telling you where it ran:
  `vm ▸ windows (Windows 11) ▸ ~/work/repo ▸ $ cargo test` … `vm ▸ windows ▸ exit 0 ▸ 12s`.
  Stdout is the command's own output, untouched — so redirects and pipes on the
  host side are clean: `vm exec linux -- 'cargo test 2>&1' > log.txt` captures a
  big log to a file (grep the file instead of paging the log through your
  context) with no vm chatter mixed in.
- vm's own stdin is **never** forwarded to the guest: `echo data | vm exec …`
  runs the command with no input — `cat > f` writes an *empty* file, and exits 0.
  vm prints a `vm ▸ note:` when it spots piped stdin. To feed a command data,
  write it to a file in the repo first — the sync carries it.

## Commands

```sh
vm ls                          # aliases, VM state, guest checkout paths
vm exec <alias> -- <cmd>…      # sync repo, run cmd in the guest checkout
vm exec <alias> --no-sync -- <cmd>…     # skip sync (state queries, quick re-runs)
vm exec <alias> --writeback -- <cmd>…   # pull guest file changes back to host
                                        # (for guest-side fixers: clippy --fix, fmt)
vm exec <alias> -e NAME=value -- <cmd>… # set env var for the guest command;
                                        # -e NAME forwards the host's value.
                                        # Repeatable; put -e before the --
vm exec <alias> --with-file .env -- <cmd>…  # sync a GITIGNORED file too (a plain
                                        # sync leaves it on the host). Repeatable;
                                        # it stays in the guest only as long as the
                                        # flag does (see Gitignored files)
vm exec <alias> --with-snapshot -- <cmd>…  # snapshot, run, roll back — the guest
                                        # ends up untouched (reverts EVERYTHING
                                        # since the snapshot, including the pre-run
                                        # sync; needs the VM to itself and ~2×VM-RAM
                                        # free disk). For destructive experiments
vm exec <alias> --guest-env none -- <cmd>…  # run the bare command: no mise setup,
                                        # no `mise exec --` wrap (see Guest env)
vm exec <alias> -- '<script>'  # ONE argument = a shell script in the guest:
                               # pipes, &&, redirects, cd. Several args = exec'd
                               # as given, byte-identical, no shell (see below)
vm exec --or-native <os> -- <cmd>…      # run natively (no VM) IF the host is already
                                        # that OS, else route to the VM. The target
                                        # must literally be windows|linux|macos so it
                                        # works config-free on CI. Omit to force the VM
vm sync <alias>                # sync only (--with-file works here too)
vm reap [alias] [--idle-minutes N]  # suspend VMs idle ≥N min (default 30) and not in
                                    # use; --install/--uninstall manage a launchd job
                                    # that runs it every 5 min (--install bakes in the
                                    # --idle-minutes you pass alongside it)
vm claude <alias> "<prompt>" [claude flags…]  # headless `claude -p` in the guest
                                     # checkout; the VM is the permission boundary.
                                     # Writeback of source edits is ON by default
                                     # (--no-writeback opts out); --with-snapshot
                                     # rolls the guest back afterwards; -e forwards
                                     # env vars. vm's own flags go BEFORE the prompt
vm deploy <alias>              # rebuild + install the guest agent (after vm src changes)
vm doctor [alias]              # check prlctl/config/ssh/agent/git per guest. Naming an
                               # alias brings that VM up and runs the live checks; the
                               # bare form surveys all VMs and skips the ones that are
                               # down. Exits 1 if any check fails, 2 on an unknown alias
vm shot <alias> [file.png]     # screenshot the VM display (see GUI dialogs ssh can't)
vm clean <alias>               # delete the guest checkout of this repo (next sync recreates)
```

**There is no `vm start` and no `vm stop` — never go looking for them.** VM
lifecycle is not your problem: every command that needs a guest (`exec`, `sync`,
`claude`, `deploy`, `clean`, `shot`, `doctor <alias>`) starts or resumes the VM
itself and tells you it is doing so, and `vm reap` suspends VMs nobody is using.
So a suspended VM is not a blocker to clear first — just run the command you
actually wanted:

```
vm ▸ linux ▸ 'Ubuntu 24.04' is suspended — resuming it…
vm ▸ linux ▸ ready at 10.211.55.4 ▸ 3.0s
```

A resume is ~1–3s (a cold macOS boot is longer). A VM that is already running
prints none of this — silence there means there was nothing to wait for.

**`vm` executes in a VM by default — never on the host.** Even `vm exec macos`
on a macOS host goes to the macOS VM. The one opt-in exception is
`--or-native`: with it, a command whose target os already matches the host runs
natively (no VM, no sync), announced by a `vm ▸ native (…)` breadcrumb so the
location is never hidden. This is for one task line that must work both on a dev
host (→ guest) and on a CI runner that is already the target OS (→ native, where
there is no Parallels or config) — which is why the target there must literally
be `windows`/`linux`/`macos`: that name is matched against the host *before* the
config is read. Omit `--or-native` to force the VM even on a matching host (e.g.
a macOS host driving the macOS guest for UI tests).

- **Command form is decided by how many arguments you pass** (like a Dockerfile
  `RUN`). **Several** arguments are exec'd exactly as given — they arrive in the
  guest byte-identical (JSON to a guest agent, no shell quoting layer), so
  `vm exec linux -- grep 'a|b' src/lib.rs` keeps its regex. **Exactly one**
  argument is a *script*, run by the guest's own shell (`sh -c`, or `cmd /C` on
  Windows), which is how you get pipes, `&&`, redirects and builtins:
  `vm exec linux -- 'cd src && cargo test'`. There is no `--shell` flag (it was
  removed; passing it is an exit-2 error that tells you this).
  - Getting it wrong is usually harmless but silent: `-- echo a '&&' echo b` is
    five arguments, so `&&` is printed by echo, not obeyed. vm prints a
    `vm ▸ note:` when it spots that; the note is advice on stderr and never
    changes what runs.
  - The `$ …` breadcrumb always shows the **literal** command the guest runs —
    wrap and shell included — so you can always see which form you got.
- `-e NAME=value` sets an env var for the guest process, and `-e NAME` forwards
  the host's current value (errors if unset). It rides the same JSON channel as
  the args — identical on `cmd` and `sh` guests, no script form needed, process-
  scoped. Don't forward `PATH` (it would shadow the guest's own tool paths).
- Commands run where GUI automation works on every OS: Windows exec rides
  `prlctl exec` into the console session (ssh would land in session 0 with an
  empty desktop); unix guests go over ssh, which reaches AT-SPI / the AX API
  directly. So UIA/accessibility tests need no special flag — plain `vm exec`.
- Exit codes propagate: a guest command's own exit code is what `vm` returns.
  vm's *own* failures are reserved so a caller can tell "the command failed" from
  "vm hiccuped": `125` = infra error (sync/agent/ssh/VM lifecycle; often
  transient, may be worth a retry), `2` = usage/config error (bad alias,
  missing/invalid config, not in a git repo — fix it, don't retry). Caveats:
  `255` and `127` stay ambiguous (ssh conn-fail vs a guest exit 255; agent
  missing vs command-not-found). Because 255 may mean the connection dropped,
  `--writeback` skips the writeback on that code (and prints that it did).
  Killing `vm` (Ctrl-C, SIGKILL, closed session) kills the whole process tree in
  the guest — no orphaned compilers.

## Gitignored files (`.env` and friends)

The sync carries what git sees. Gitignored files stay on the host — deliberately
(that is what keeps `target/` and `node_modules` from crossing), but it means a
build that reads a gitignored `.env` fails in the guest while every breadcrumb
reads green. Two fixes, in order of preference:

```sh
vm exec lin -e API_KEY -e DATABASE_URL -- cargo test   # values only; nothing is
                                                       # written to guest disk.
                                                       # -e NAME forwards the host's
                                                       # value, -e NAME=v sets it
vm exec lin --with-file .env -- cargo test             # the file itself, when the
                                                       # build insists on reading it
```

- `--with-file` rides the ordinary snapshot, so the file is tree-hash-verified
  like any other. It is in the guest **iff** the last sync named it: run without
  the flag and the next sync takes it back out. Repeat the flag for more files.
- Its contents *do* land on the guest's disk (and in git objects on both sides).
  When that matters, `-e` is the one that does not.
- Refused up front (exit 2): a path that does not exist, a directory, a symlink
  (git would sync the link, not the file), or a path outside the repo.
- **When a guest command fails and a gitignored `.env*` stayed behind, vm prints
  a `vm ▸ note:` saying so.** If you see it, that is very likely your failure —
  re-run with `-e` or `--with-file` before investigating anything else.

## Guest environments

A guest checkout is a fresh copy of the repo, so a repo whose tools are managed
by **mise** needs `mise trust` there once, and its commands need `mise exec --`
in front. `vm` does both automatically when it detects a mise config at the repo
root (`mise.toml`, `.mise.toml`, `.config/mise/config.toml`, …), and **says so**
before doing anything:

```
vm ▸ windows ▸ guest env: mise (detected mise.toml) — `mise trust` on first sync,
     exec commands wrapped `mise exec --`; --guest-env none disables
```

So `vm exec windows -- cargo test` really runs `mise exec -- cargo test` in the
guest. `vm claude` is wrapped too, so the commands Claude runs inside the guest
resolve the repo's tools. For a script (one argument) the wrap goes *around* the
shell — `mise exec -- sh -c '<script>'` — so builtins, pipes and exit codes all
behave. Override with `--guest-env mise` (force) or `--guest-env none` (bare
command, no setup, no wrap) on `exec` / `sync` / `claude`. There is **no per-repo
config file** — an older `.vm.toml` with `on_first_sync` / `wrap` is obsolete and
ignored.

**Expect the first wrapped exec in a fresh guest to be slow.** `mise exec`
installs the repo's `[tools]` before running, so even a trivial command can take
minutes the first time (it may compile things), then ~1s afterwards. vm is not
hung — mise is installing. `--guest-env none` skips the wrap entirely.

## Config

Install: prebuilt release binaries are ubi-installable — `"ubi:akesson/vm" =
"latest"` in a `mise` `[tools]` block (arm64 macOS/Linux/Windows + x86_64
Linux/Windows), or `cargo install --path .` from source. Deploy the guest agent
with `vm deploy <alias>`.

`~/.config/vm/config.toml` (or `$VM_CONFIG`) — the machine's VM inventory, and
the *only* config file:

```toml
[vm.windows]                    # the alias every command uses
parallels_name = "Windows 11"   # exact name from `prlctl list -a`
os = "windows"                  # windows | linux | macos
user = "hakesson"               # guest user for ssh
work_root = "~/work"            # guest dir holding per-repo checkouts
# host = "10.0.0.5"             # optional IP override (else discovered via prlctl)
# agent_path = "…"              # optional (else <home>/.vm/bin/vm[.exe])
```

Naming each alias for its OS is what makes `vm exec --or-native windows …`
work on both a dev host and a Windows CI runner.

## Hit a bug in vm itself? File it

If `vm` misbehaves — a command fails in a way that looks like a tool bug, these
docs don't match what actually happens, or an obviously-needed workflow is
missing — open an issue so it gets fixed (search existing ones first to avoid a
dup):

```sh
gh issue create --repo akesson/vm --title "<short summary>" \
  --body "<cmd you ran · what happened · what you expected · guest OS/target>"
```

Scope this to problems with `vm` the tool, **not** the project you're using it
on. Paste the failing `vm ▸ …` breadcrumb line — it names the guest and command.

## Gotchas

- Nothing to clean up when you're done: you never started a VM, so don't try to
  put one away. The reap timer suspends idle VMs on its own, and the next
  command resumes whatever it needs. Parallel `vm` invocations are safe: uses
  hold a shared per-VM lock, so `--with-snapshot`/reap won't fire while another
  vm process is using the VM. Parallel `vm exec` of the *same* repo to the
  *same* guest (e.g. a `mise` fan-out) is safe too — the syncs serialize behind
  a per-(repo, guest) lock, so a second one waits a moment for the first rather
  than racing on the shared git snapshot.
- A wait for a VM is never silent: the wake is announced, past 10s vm prints
  where it has got to (`vm ▸ macos ▸ 'macOS' running, no IP yet — 10s of 90s`),
  and readiness closes it out. If a VM is not coming up at all — a resume that
  Parallels reported as successful but did not perform, or something suspending
  it again underneath — vm says so within ~15s (exit 125) instead of waiting the
  guest out. `vm ls` shows every VM's status and IP, so a wait can be watched
  from another terminal.
- A lone path with a **space in its filename** is the one place the one-argument
  form bites: it is a script, so the shell splits it. Quote it for the shell
  (`vm exec macos -- '"/Applications/My App/run"'`) or pass it in exec form. vm
  notes this when the file actually exists.
- `--writeback` applies the guest diff to the **host working tree** as a patch;
  only use it for commands that deliberately edit sources.
- Sync pushes bypass git hooks by design (`--no-verify`) — they are replication,
  not publishing.
- macOS guests (Apple Silicon): snapshots need Parallels 20+ with
  macOS 14+ on host and guest.
  Full Xcode is required in the guest for xcodebuild/XCUITest;
  SPM `swift test` works with just the Command Line Tools.
- Windows exec needs the config user logged in at the VM console (it runs in
  that session); `vm doctor` checks this ("console user" / "console agent").
- "guest agent missing/outdated" errors → `vm deploy <alias>`.
