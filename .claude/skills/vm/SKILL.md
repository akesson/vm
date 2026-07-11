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
- The guest checkout lives at `<work_root>/<repo-name>` (e.g. `~/work/myrepo`).
  It is a *replica*: syncs `reset --hard` + `clean -fd` it. Never hand-edit it;
  gitignored files (build caches) survive syncs.
- Every run prints breadcrumbs to stderr telling you where it ran:
  `vm ▸ win (Windows 11) ▸ ~/work/repo ▸ $ cargo test` … `vm ▸ win ▸ exit 0 ▸ 12s`.
  Stdout is the command's own output, untouched.

## Commands

```sh
vm ls                          # aliases, VM state, guest checkout paths
vm exec <target> -- <cmd>…     # sync repo, run cmd in guest checkout;
                               # target = alias OR os name (windows|linux|macos)
vm exec <target> --no-sync -- <cmd>…    # skip sync (state queries, quick re-runs)
vm exec <target> --writeback -- <cmd>…  # pull guest file changes back to host
                                        # (for guest-side fixers: clippy --fix, fmt)
vm exec <target> -e NAME=value -- <cmd>…  # set env var for the guest command;
                                          # -e NAME forwards the host's value.
                                          # Repeatable; put -e before the --
vm exec --or-native <os> -- <cmd>…  # run natively (no VM) IF the host is already
                                    # that OS, else route to the VM. Use an os name
                                    # (windows|linux|macos) so it works config-free
                                    # on CI. Omit it to force the VM.
vm sync <alias>                # sync only
vm start|stop <alias>          # lifecycle (start waits for ssh; stop refuses while
                               # other vm processes use the VM — --force overrides;
                               # --kill hard-powers-off instead of graceful shutdown)
vm reap [alias] [--idle-minutes N]  # suspend VMs idle ≥N min (default 30) and not in
                                    # use; --install/--uninstall manage a launchd job
                                    # that runs it every 5 min
vm claude <target> "<prompt>" [claude flags…]  # headless `claude -p` in the guest
                                     # checkout; the VM is the permission boundary.
                                     # Writeback of source edits is ON by default
                                     # (--no-writeback opts out); --with-snapshot
                                     # rolls the guest back afterwards
vm deploy <alias>              # rebuild + install the guest agent (after vm src changes)
vm doctor [alias]              # check prlctl/config/ssh/agent/git per guest (read-only)
vm shot <alias> [file.png]     # screenshot the VM display (see GUI dialogs ssh can't)
vm clean <alias>               # delete the guest checkout of this repo (next sync recreates)
vm with-snapshot <target> -- <cmd>…  # snapshot, run, roll back — guest ends up
                                     # untouched (reverts EVERYTHING since the
                                     # snapshot, including the pre-run sync; needs
                                     # the VM to itself and ~2×VM-RAM free disk)
```

**`vm` executes in a VM by default — never on the host.** Even `vm exec macos`
on a macOS host goes to the macOS VM. The one opt-in exception is
`--or-native`: with it, a command whose target os already matches the host runs
natively (no VM, no sync), announced by a `vm ▸ native (…)` breadcrumb so the
location is never hidden. This is for one task line that must work both on a dev
host (→ guest) and on a CI runner that is already the target OS (→ native, where
there is no Parallels or config). Use an os name as the target there so the
native match needs no config; omit `--or-native` to force the VM even on a
matching host (e.g. a macOS host driving the macOS guest for UI tests).

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
  transient, may be worth a retry), `2` = usage/config error (bad target,
  missing/invalid config, not in a git repo — fix it, don't retry). Caveats:
  `255` and `127` stay ambiguous (ssh conn-fail vs a guest exit 255; agent
  missing vs command-not-found). Killing `vm` (Ctrl-C, SIGKILL, closed session)
  kills the whole process tree in the guest — no orphaned compilers.

## Config

Install: prebuilt release binaries are ubi-installable — `"ubi:akesson/vm" =
"latest"` in a `mise` `[tools]` block (arm64 macOS/Linux/Windows + x86_64
Linux/Windows), or `cargo install --path .` from source. Deploy the guest agent
with `vm deploy <alias>`.

`~/.config/vm/config.toml` — machine-level VM inventory:

```toml
[vm.win]
parallels_name = "Windows 11"
os = "windows"          # windows | linux | macos
user = "hakesson"
work_root = "~/work"
# host = "10.0.0.5"     # optional IP override (else discovered via prlctl)
```

`.vm.toml` — optional, committed at the **repo root**, so the repo declares the
one-time setup its guest checkout needs:

```toml
# Runs once in the guest checkout the first time it is created — and again after
# `vm clean` or any other checkout recreation — before the exec'd command. A
# nonzero exit fails the run (exit 125). Keep it to a simple command: it runs
# under the guest shell (`sh -c` on unix, `cmd /C` on Windows).
on_first_sync = "mise trust"

# Prefix prepended to every guest `vm exec` / `vm with-snapshot` command (argv
# space, no extra quoting), so a mise-managed guest checkout resolves its tools.
# Guest path only — a `--or-native` run already has the launching env, and
# `vm claude` is not wrapped.
wrap = ["mise", "exec", "--"]
```

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
  are safe: uses hold a shared per-VM lock; stop/with-snapshot/reap won't
  fire while another vm process is using the VM. Parallel `vm exec` of the
  *same* repo to the *same* guest (e.g. a `mise` fan-out) is safe too — the
  syncs serialize behind a per-(repo, guest) lock, so a second one waits a
  moment for the first rather than racing on the shared git snapshot.
- First run of a mise-managed repo in a fresh guest checkout needs `mise trust`.
  Set `on_first_sync = "mise trust"` in a committed `.vm.toml` (see Config) and
  vm runs it automatically the first time each checkout is created — no manual
  step. (Or, one-off: `vm exec <alias> --no-sync -- mise trust`.)
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
