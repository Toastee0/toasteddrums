# cargo build --release, never blocked by a copy that is already running.
#
# WHY: Windows keeps a running image file open, so `cargo build --release` dies with
# "Access is denied (os error 5)" whenever anything is holding
# target\release\toasteddrums.exe -- a live MCP server, the `ui` window, a stray `live`
# session. For this project that is the normal state, not an edge case: editing the tracker
# while an agent or the UI is connected to it is the whole point.
#
# THE TRICK: Windows locks an executable's CONTENTS but not its directory entry. A running
# exe can be RENAMED. The process carries on happily under the new name, the original path
# is freed, and cargo links a fresh binary into it. This is how self-updating Windows
# programs replace themselves. Parked copies are swept on a later build, once their process
# has exited and the file stops being locked.
#
# Usage: .\kit\build.ps1            (or with any extra cargo arguments appended)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$dir = Join-Path $root 'target\release'
$exe = Join-Path $dir 'toasteddrums.exe'

if (Test-Path $exe) {
    try {
        # Nothing is holding it: just clear the path.
        Remove-Item $exe -ErrorAction Stop
    } catch {
        # Held by a running process. Park it and let that process keep going.
        $parked = Join-Path $dir ("toasteddrums-inuse-{0}.exe" -f (Get-Date -Format 'HHmmssfff'))
        Move-Item $exe $parked -ErrorAction Stop
        Write-Host "build: a running copy held the binary; parked it as $(Split-Path $parked -Leaf)"
    }
}

# Sweep parked copies whose process has since exited. Ones still running are still locked,
# and are skipped without complaint.
if (Test-Path $dir) {
    Get-ChildItem $dir -Filter 'toasteddrums-inuse-*.exe' -ErrorAction SilentlyContinue |
        ForEach-Object { try { Remove-Item $_.FullName -ErrorAction Stop } catch {} }
}

Push-Location $root
try { cargo build --release @args } finally { Pop-Location }
exit $LASTEXITCODE
