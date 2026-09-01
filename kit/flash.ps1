# flash.ps1 — flash the prebuilt pads firmware to the XIAO ESP32-S3 (ReSpeaker Lite carrier).
# Fast path: no toolchain needed, just esptool + the merged image built on coffee0.
#   .\flash.ps1                 auto-detect the XIAO's port
#   .\flash.ps1 -Port COM7      force a port
param([string]$Port = "", [string]$Image = "$PSScriptRoot\pads.merged.bin")

$ErrorActionPreference = "Stop"

# The XIAO ESP32-S3's native USB-serial-JTAG enumerates as 303A:1001.
# (The annunciator's XIAO RP2040 is on COM3 and must NOT be touched — it is a
#  different VID:PID, but always confirm the port you are about to write to.)
if (-not $Port) {
  $dev = Get-PnpDevice -Class Ports -PresentOnly |
         Where-Object { $_.InstanceId -match "VID_303A&PID_1001" } | Select-Object -First 1
  if (-not $dev) { throw "No 303A:1001 device found. Plug the XIAO's own USB-C in (not the XMOS port by the 3.5mm jack)." }
  if ($dev.FriendlyName -match "\((COM\d+)\)") { $Port = $Matches[1] } else { throw "Could not parse a COM port from '$($dev.FriendlyName)'" }
  Write-Host "found XIAO ESP32-S3 on $Port  ($($dev.FriendlyName))"
}
if ($Port -eq "COM3") { throw "COM3 is the desk annunciator. Refusing." }

$esptool = Get-Command esptool -ErrorAction SilentlyContinue
if (-not $esptool) {
  Write-Host "installing esptool 4.8.1 via uv..."
  uv tool install "esptool==4.8.1"
  $esptool = Get-Command esptool -ErrorAction Stop
}

# Merged image covers bootloader + partitions + app, so a single write at 0x0.
& $esptool.Source --chip esp32s3 --port $Port --baud 921600 write_flash 0x0 $Image
Write-Host "`nflashed. Now: .\mon.ps1 -Port $Port"
