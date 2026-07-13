# Windows Update via the WUA COM API — what PSWindowsUpdate wraps, minus the
# module install. Runs headless as SYSTEM. Software updates only (no drivers).
# NEVER reboots: it reports RebootRequired and leaves the decision to a human.
#
# Feed this to the guest on STDIN, never as an argument:
#   vm run windows --elevated -- powershell -NoProfile -NonInteractive -Command - < windows-update.ps1

$ProgressPreference = 'SilentlyContinue'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
trap { [Console]::Error.WriteLine($_.Exception.Message); exit 1 }
$ErrorActionPreference = 'Stop'

function Say($msg) { [Console]::Out.WriteLine($msg); [Console]::Out.Flush() }

$names = @{ 0 = 'not started'; 1 = 'in progress'; 2 = 'succeeded';
            3 = 'succeeded with errors'; 4 = 'failed'; 5 = 'aborted' }

# Say this BEFORE touching COM: creating the session can block for minutes while
# the Windows Update service is busy, and a line printed after it never prints —
# leaving a step that has said nothing, which reads as a hang.
Say 'searching for updates (minutes, and silent while Windows Update works)...'

$session  = New-Object -ComObject Microsoft.Update.Session
$searcher = $session.CreateUpdateSearcher()
$result   = $searcher.Search("IsInstalled=0 and Type='Software' and IsHidden=0")

if ($result.Updates.Count -eq 0) { Say 'no updates available'; exit 0 }

$updates = New-Object -ComObject Microsoft.Update.UpdateColl
foreach ($u in $result.Updates) {
  Say ('  ' + $u.Title)
  if (-not $u.EulaAccepted) { $u.AcceptEula() | Out-Null }
  $updates.Add($u) | Out-Null
}

Say 'downloading...'
$downloader = $session.CreateUpdateDownloader()
$downloader.Updates = $updates
$downloader.Download() | Out-Null

Say 'installing...'
$installer = $session.CreateUpdateInstaller()
$installer.Updates = $updates
$r = $installer.Install()

for ($i = 0; $i -lt $updates.Count; $i++) {
  Say ('  ' + $updates.Item($i).Title + ' -> ' + $names[[int]$r.GetUpdateResult($i).ResultCode])
}

if ($r.RebootRequired) { Say 'RESTART REQUIRED - reboot the guest to finish' }

# 2 = succeeded, 3 = succeeded with errors. Anything else is a real failure.
if ($r.ResultCode -ne 2 -and $r.ResultCode -ne 3) { exit 1 }
