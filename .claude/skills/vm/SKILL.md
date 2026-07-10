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
vm sync <alias>                # sync only
vm start|stop <alias>          # lifecycle (start waits for ssh; stop refuses while
                               # other vm processes use the VM — --force overrides)
vm reap [alias] [--idle-minutes N]  # suspend VMs idle ≥N min (default 30) and not in
                                    # use; --install adds a launchd job every 5 min
vm deploy <alias>              # rebuild + install the guest agent (after vm src changes)
vm doctor [alias]              # check prlctl/config/ssh/agent/git per guest (read-only)
vm shot <alias> [file.png]     # screenshot the VM display (see GUI dialogs ssh can't)
vm clean <alias>               # delete the guest checkout of this repo (next sync recreates)
vm with-snapshot <target> -- <cmd>…  # snapshot, run, roll back — guest ends up
                                     # untouched (reverts EVERYTHING since the
                                     # snapshot, including the pre-run sync; needs
                                     # the VM to itself and ~2×VM-RAM free disk)
```

**`vm` always executes in a VM — never on the host.** Even `vm exec macos`
on a macOS host goes to the macOS VM. To run something natively, just run it;
scripts that choose between native and VM do their own OS check.

- Args after `--` arrive in the guest byte-identical (JSON over ssh, no shell
  quoting layer). `--shell` runs the command through `sh -c` / `cmd /C` instead.
- Exit codes propagate. Killing `vm` (Ctrl-C, SIGKILL, closed session) kills
  the whole process tree in the guest — no orphaned compilers.

## Config

`~/.config/vm/config.toml`:

```toml
[vm.win]
parallels_name = "Windows 11"
os = "windows"          # windows | linux | macos
user = "hakesson"
work_root = "~/work"
# host = "10.0.0.5"     # optional IP override (else discovered via prlctl)
```

## Gotchas

- Don't stop VMs when done — the reap timer suspends idle VMs automatically,
  and any `vm exec` resumes a suspended VM in ~1s. Parallel `vm` invocations
  are safe: uses hold a shared per-VM lock; stop/with-snapshot/reap won't
  fire while another vm process is using the VM.
- First run of a mise-managed repo in a fresh guest checkout:
  `vm exec <alias> --no-sync -- mise trust` (then tools auto-install on first task).
- `--writeback` applies the guest diff to the **host working tree** as a patch;
  only use it for commands that deliberately edit sources.
- Sync pushes bypass git hooks by design (`--no-verify`) — they are replication,
  not publishing.
- macOS guests (Apple Silicon): no `prlctl exec` — ssh only. Snapshots need
  Parallels 20+ with macOS 14+ on host and guest.
  Full Xcode is required in the guest for xcodebuild/XCUITest;
  SPM `swift test` works with just the Command Line Tools.
- "guest agent missing/outdated" errors → `vm deploy <alias>`.
