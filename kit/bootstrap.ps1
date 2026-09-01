# bootstrap.ps1 — put a full firmware build loop on phobos-lt, so edit -> build -> flash
# never leaves the laptop. Pinned to the SAME core coffee0 uses (3.3.11) so a binary built
# here is the binary built there. Downloads ~1 GB the first time; safe to re-run.
$ErrorActionPreference = "Stop"

if (-not (Get-Command arduino-cli -ErrorAction SilentlyContinue)) {
  Write-Host "installing arduino-cli..."
  winget install --id ArduinoSA.CLI --accept-source-agreements --accept-package-agreements
  $env:PATH = "$env:PATH;$env:LOCALAPPDATA\Microsoft\WinGet\Links"
}

arduino-cli config init --overwrite
arduino-cli config add board_manager.additional_urls https://espressif.github.io/arduino-esp32/package_esp32_index.json
arduino-cli core update-index
# Pin the version. Unpinned, a core bump silently changes the touch driver under us —
# 3.3.x already moved touch to the NG driver once (memory: supply-chain-version-caution).
arduino-cli core install esp32:esp32@3.3.11

Write-Host "`nbuild + flash from here with:"
Write-Host '  arduino-cli compile -b esp32:esp32:XIAO_ESP32S3:USBMode=hwcdc,CDCOnBoot=cdc,PSRAM=opi .\pads'
Write-Host '  arduino-cli upload  -b esp32:esp32:XIAO_ESP32S3:USBMode=hwcdc,CDCOnBoot=cdc,PSRAM=opi -p COMn .\pads'
