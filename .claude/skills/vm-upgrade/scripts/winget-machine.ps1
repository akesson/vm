# Machine-scope winget, run as SYSTEM via `vm run --elevated`.
#
# winget is an MSIX app, so SYSTEM's PATH has never heard of it — the exe has to
# be resolved under WindowsApps by hand. Only the `winget` source: `msstore`
# needs a user context SYSTEM does not have.
#
# Feed this to the guest on STDIN:
#   vm run windows --elevated -- powershell -NoProfile -NonInteractive -Command - < winget-machine.ps1
#
# This pass sees ONLY machine-scope packages (Git, VC++ redists, VS Build Tools,
# PowerShell). The user's own installs — mise, rustup, winget itself, claude —
# are invisible to SYSTEM and need the separate user-scope pass. See SKILL.md.

$ProgressPreference = 'SilentlyContinue'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
trap { [Console]::Error.WriteLine($_.Exception.Message); exit 1 }

function Say($msg) { [Console]::Out.WriteLine($msg); [Console]::Out.Flush() }

$w = Get-ChildItem 'C:\Program Files\WindowsApps\Microsoft.DesktopAppInstaller_*__8wekyb3d8bbwe\winget.exe' -ErrorAction SilentlyContinue |
     Sort-Object { [version]$_.VersionInfo.FileVersion } |
     Select-Object -Last 1

if (-not $w) { Say 'winget not found under WindowsApps - skipped'; exit 0 }

& $w.FullName upgrade --all --silent --source winget `
    --accept-package-agreements --accept-source-agreements --disable-interactivity

# -1978335189 (0x8A15002B) is winget's "nothing to upgrade" — a success here, and
# it must be folded into 0 inside PowerShell: an HRESULT does not survive a
# shell's 8-bit exit status.
if ($LASTEXITCODE -eq 0 -or $LASTEXITCODE -eq -1978335189) { exit 0 }
exit 1
