---
name: vm
description: Run commands in Parallels VMs (Windows/Linux/macOS) from any git repo. Use when a build/test/lint must run on another OS, or when the repo's scripts wrap `vm exec`. Handles code sync automatically — no shared folders, no manual copying.
---

# vm — cross-VM exec & sync

`vm` runs a command inside a Parallels guest **against a guest-local checkout
of the current repo**. Before every exec it snapshots the host working tree
(including uncommitted + untracked files, excluding gitignored ones) and syncs
it to the guest via git objects. You never copy files yourself.

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
  Stdout is the command's own output, untouched.

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
vm exec <alias> --with-snapshot -- <cmd>…  # snapshot, run, roll back — the guest
                                        # ends up untouched (reverts EVERYTHING
                                        # since the snapshot, including the pre-run
                                        # sync; needs the VM to itself and ~2×VM-RAM
                                        # free disk). For destructive experiments
vm exec <alias> --guest-env none -- <cmd>…  # run the bare command: no mise setup,
                                        # no `mise exec --` wrap (see Guest env)
vm exec --or-native <os> -- <cmd>…      # run natively (no VM) IF the host is already
                                        # that OS, else route to the VM. The target
                                        # must literally be windows|linux|macos so it
                                        # works config-free on CI. Omit to force the VM
vm sync <alias>                # sync only
vm start|stop <alias>          # lifecycle (start waits for ssh; stop refuses while
                               # other vm processes use the VM — --force overrides;
                               # --kill hard-powers-off instead of graceful shutdown)
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
vm doctor [alias]              # check prlctl/config/ssh/agent/git per guest (read-only;
                               # exits 1 if any check fails, 2 on an unknown alias)
vm shot <alias> [file.png]     # screenshot the VM display (see GUI dialogs ssh can't)
vm clean <alias>               # delete the guest checkout of this repo (next sync recreates)
```

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

- Args after `--` arrive in the guest byte-identical (JSON to a guest agent,
  no shell quoting layer). `--shell` runs the command through `sh -c` /
  `cmd /C` instead.
- `-e NAME=value` sets an env var for the guest process, and `-e NAME` forwards
  the host's current value (errors if unset). It rides the same JSON channel as
  the args — identical on `cmd` and `sh` guests, no `--shell` needed, process-
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
resolve the repo's tools. With `--shell`, the wrap goes *around* the shell
(`mise exec -- sh -c '<script>'`), so builtins, pipes and exit codes all behave.
Override with `--guest-env mise` (force) or `--guest-env none` (bare command, no
setup, no wrap) on `exec` / `sync` / `claude`. There is **no per-repo config
file** — an older `.vm.toml` with `on_first_sync` / `wrap` is obsolete and
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

- Don't stop VMs when done — the reap timer suspends idle VMs automatically,
  and any `vm exec` resumes a suspended VM in ~1s. Parallel `vm` invocations
  are safe: uses hold a shared per-VM lock; stop/`--with-snapshot`/reap won't
  fire while another vm process is using the VM. Parallel `vm exec` of the
  *same* repo to the *same* guest (e.g. a `mise` fan-out) is safe too — the
  syncs serialize behind a per-(repo, guest) lock, so a second one waits a
  moment for the first rather than racing on the shared git snapshot.
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
