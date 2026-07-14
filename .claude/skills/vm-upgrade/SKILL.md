---
name: vm-upgrade
description: Weekly maintenance sweep of the Parallels dev guests — OS updates and installed dev tooling (apt/snap, Homebrew + Apple updates, winget + Windows Update, plus rustup/mise/claude) run inside each VM. Use for /vm-upgrade, "update the VMs", "patch the guests", or any routine VM maintenance.
---

# vm-upgrade — weekly maintenance of the dev guests

Run each guest's **own** updaters — the ones you would run by hand if you logged
in — then report what changed and what needs the human. The guests are dev
machines nobody logs into, which is exactly how they end up months behind.

**Never reboot a guest, never answer a prompt on the user's behalf, never
install something they did not already have.** When a step needs a decision or a
password, stop and ask. The list of those cases is at the bottom, and it is the
most important part of this skill.

Work through the guests one at a time and narrate as you go. The whole sweep is
~2–15 minutes when there is little to do; a big Windows or Xcode update can take
far longer.

## 1. Inventory

```sh
vm ls                                   # aliases, OS, status (works outside a git repo)
```

That is all the setup there is. **Do not resume guests, poll for IPs, or touch
lock files** — `vm run` brings a guest up itself, says so, and holds the VM's use
lock for the whole command, so `vm reap` cannot shut one down mid-sweep. Aliases are
`linux`, `macos`, `windows`.

## 2. The two channels

Everything below goes through exactly one of these. Pick by **whose** software it
is, not by convenience.

| | how | runs as | for |
|---|---|---|---|
| **user** | `vm run <alias> -- …` | the configured user | brew, rustup, mise, claude, **user-scope winget** |
| **elevated** | `vm run <alias> --elevated -- …` | root (Linux/macOS), SYSTEM (Windows) | apt, snap, softwareupdate, **machine-scope winget**, Windows Update |

```sh
vm run linux -- rustup update                        # user channel
vm run linux --elevated -- apt-get update            # elevated channel
vm run linux --elevated -- sh < step.sh              # a script: on stdin (see §3)
```

- **`--elevated` is the only elevation that works.** `sudo` over ssh wants a
  password, and the Windows guest user is **not an administrator** (UAC cannot be
  satisfied headless). `--elevated` rides Parallels Tools and is already
  root/SYSTEM.
- **The two channels have different PATHs, and that is the point.** The user
  channel resolves `brew`, `mise`, `rustup`, `claude` (verified on all three
  guests — no PATH prefix needed). The elevated channel gets the *system* PATH:
  `apt`, `snap`, `softwareupdate`, `winget`, and nothing installed under the
  user's home. A user tool run `--elevated` will 127; a system tool run
  unelevated will refuse. Neither is worth debugging — just pick the right
  channel from the table.
- Exit codes are the command's own, so `&&` and `if` work normally.

## 3. Scripts go in on stdin

Anything longer than one command goes in a file and rides stdin — **never argv**:

```sh
cat > /tmp/step.sh <<'EOF'
export DEBIAN_FRONTEND=noninteractive
apt-get update -q && apt-get upgrade -yq && apt-get autoremove -yq
EOF
vm run linux --elevated -- sh < /tmp/step.sh                       # exit code is the script's
vm run windows --elevated -- powershell -NoProfile -NonInteractive -Command - < step.ps1
```

Why it must be stdin, and not merely why it is nicer: **`prlctl exec` — which is
what `--elevated` rides — hangs forever, silently and immune to SIGTERM, once its
*total* command line passes ~4 KB.** The limit is the combined size of all
arguments, not any single one (measured on Parallels 26.4: ten 500-byte arguments
wedge it exactly like one 5000-byte argument), so "keep each argument small" is
not a defense. vm now refuses to build such a command line rather than let it
hang — but a raw `prlctl exec` you write yourself has no such guard. On stdin
there is no limit at all: the payload rides inside the request, and 8 MiB is the
cap.

If you ever do hang a raw `prlctl exec`: it ignores SIGTERM — use `kill -9`.

## 4. What to run in each guest

Run the steps in order. A step failing does not stop the others — collect the
failures and report them at the end. A tool the guest does not have is a **skip,
not a failure** (guard with `command -v <tool> >/dev/null || exit 0`).

### linux (elevated: apt, snap · user: rustup, mise, claude)
```sh
vm run linux --elevated -- sh < step.sh   # apt: DEBIAN_FRONTEND=noninteractive, then
                                          # apt-get update -q && apt-get upgrade -yq
                                          # && apt-get autoremove -yq
vm run linux --elevated -- snap refresh
vm run linux -- rustup update
vm run linux -- 'mise self-update -y && mise upgrade'   # mise owns its binary here
vm run linux -- claude update
vm run linux --elevated -- 'test -f /var/run/reboot-required && cat /var/run/reboot-required.pkgs'
                                          # REPORT it; never reboot a guest
```

### macos (elevated: softwareupdate · user: brew, rustup, mise, claude)
```sh
vm run macos -- 'brew update && brew upgrade && brew cleanup'  # brew REFUSES to run as root
vm run macos -- rustup update
vm run macos -- mise upgrade              # brew owns the binary; do NOT self-update
vm run macos -- claude update
vm run macos --elevated -- softwareupdate --install --all      # and NEVER --restart
```

### windows (elevated: winget machine-scope, Windows Update · user: winget user-scope, rustup, mise, claude)

The two PowerShell scripts ship with this skill — **use them as they are**, they
already handle every trap in §5. `SK=~/.claude/skills/vm-upgrade` (the skill
directory; it is a symlink into the `vm` repo).

```sh
vm run windows --elevated -- powershell -NoProfile -NonInteractive -Command - < $SK/scripts/winget-machine.ps1
vm run windows -- winget upgrade --all --scope user --silent --source winget \
    --accept-package-agreements --accept-source-agreements --disable-interactivity
vm run windows --elevated -- powershell -NoProfile -NonInteractive -Command - < $SK/scripts/windows-update.ps1
vm run windows -- rustup update
vm run windows -- mise upgrade            # winget owns the binary; do NOT self-update
vm run windows -- claude update           # says "Claude is managed by winget" — that is fine
```

The user-scope winget line exits **nonzero** when there is nothing to upgrade
(the -1978335189 HRESULT, truncated by the shell) — read the output, not the
status: "No installed package found matching input criteria" means done.

Note the Windows arity trap: **one** argument is `cmd /C <script>`, so a POSIX
one-liner (`'for t in …'`) fails with "was not expected at this time". Pass
Windows commands as **several** arguments, or write PowerShell and feed it on
stdin.

## 5. Hard-won details you will otherwise re-learn the hard way

**winget has two scopes and one pass sees only one of them.** SYSTEM sees
machine-scope (Git, VC++ redists, VS Build Tools, PowerShell); the user's own
installs (mise, rustup, App Installer, Teams, claude) are user-scope and
**invisible to SYSTEM**, while the machine-scope ones cannot be upgraded by a
non-admin user. Running one pass silently leaves half the guest behind. Also:
winget exits **-1978335189** (0x8A15002B) for "nothing to upgrade" — that is a
success, not a failure.

**Who owns the `mise` binary differs per guest**, and two managers must never
fight over the same file: standalone on Linux (`mise self-update` is right), but
Homebrew owns it on macOS and winget owns it on Windows — there, the package
manager's step upgrades the binary and mise only does `mise upgrade` for the
tools it manages.

**PowerShell output comes back as a wall of XML unless you prevent it.** When
stderr is a pipe — which under `vm run` it always is — PowerShell
serializes its progress, information and error streams to CLIXML (`#< CLIXML
<Objs Version=…`). In any PowerShell you write for these guests:
- `$ProgressPreference = 'SilentlyContinue'`
- `trap { [Console]::Error.WriteLine($_.Exception.Message); exit 1 }`
- **never `Write-Host`** — it writes to the information stream. Print with
  `[Console]::Out.WriteLine(...)` followed by `[Console]::Out.Flush()`.
- The **flush is not optional**: `[Console]::Out` is block-buffered to a pipe, so
  without it a slow step's narration only arrives after the wait it was meant to
  explain.
- `[Console]::OutputEncoding = [Text.Encoding]::UTF8`, or a non-English guest's
  update titles arrive as mojibake (this Windows guest is Spanish-locale).
- Say what you are about to do **before** the call that can block — creating the
  Windows Update COM session can stall for minutes.

## 6. Stop and ask the human

Do not resolve these yourself. Collect them and put them in the final report as
explicit questions.

- **A restart is required.** Linux (`/var/run/reboot-required`) or Windows (WUA
  `RebootRequired`). Report which guest and why; never reboot a VM.
- **macOS updates that need a restart.** On Apple Silicon they also need a
  volume-owner password, so `softwareupdate` cannot install them headless — it
  will report them instead. Tell the user to apply them from Software Update in
  the guest's GUI (`vm shot macos` shows the screen).
- **Homebrew is skipping formulae from untrusted taps** (`Warning: Skipping <x>:
  tap formula is not trusted`). Those packages are **not being upgraded**. List
  them and ask whether to `brew trust --formula <tap>/<formula>`.
- **apt held a package back** ("N not upgraded"). Say which.
- **A step failed twice**, or anything asks an interactive question.
- **Anything destructive** — removing packages, changing a tap, resetting a
  toolchain — even if it looks like the obvious fix.

## 7. Report

Finish with a short table: one row per guest, what actually changed (with
versions, e.g. `git 2.53 → 2.55`), and what needs the user. Lead with anything
from §6 — that is the part they have to act on. If nothing needs them, say so
plainly in one line.
