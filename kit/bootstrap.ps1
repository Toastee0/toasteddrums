# bootstrap.ps1 — put a full firmware build loop on phobos-lt, so edit -> build -> flash
# never leaves the laptop. Pinned to the SAME core coffee0 uses (3.3.11) so a binary built
# here is the binary built there. Downloads ~1 GB the first time; safe to re-run.
$ErrorActionPreference = "Stop"

if (-not (Get-Command arduino-cli -ErrorAction SilentlyContinue)) {
  Write-Host "installing arduino-cli..."
  winget install --id ArduinoSA.CLI --accept-source-agreements --accept-package-agreements
  # The MSI puts arduino-cli in Program Files and edits the *machine* PATH, which this
  # already-running shell cannot see — hence the explicit path here. Without it this
  # script printed an error and still exited 0, installing no core at all.
  $env:PATH = "$env:PATH;C:\Program Files\Arduino CLI;$env:LOCALAPPDATA\Microsoft\WinGet\Links"
}

arduino-cli config init --overwrite
arduino-cli config add board_manager.additional_urls https://espressif.github.io/arduino-esp32/package_esp32_index.json
arduino-cli core update-index
# Pin the version. Unpinned, a core bump silently changes the touch driver under us —
# 3.3.x already moved touch to the NG driver once (memory: supply-chain-version-caution).
arduino-cli core install esp32:esp32@3.3.11

Write-Host "`nbuild + flash from here with:"
Write-Host '  arduino-cli compile -b esp32:esp32:XIAO_ESP32S3 .\pads'
Write-Host '  arduino-cli upload  -b esp32:esp32:XIAO_ESP32S3 -p COMn .\pads'
