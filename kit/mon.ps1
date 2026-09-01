# mon.ps1 — read the pads bench stream and log it. arduino-cli's monitor was flaky on the
# old laptop, so this uses .NET SerialPort directly (memory: respeaker-lite-sonar).
#   .\mon.ps1                        watch, log to pads-<timestamp>.csv
#   .\mon.ps1 -Seconds 20            capture 20 s and stop
#   .\mon.ps1 -Send "z"              send a command (z re-zero, s stream toggle,
#                                    "c 32 256" timing, "g 500" charge times)
param([string]$Port = "", [int]$Seconds = 0, [string]$Send = "", [string]$Log = "")

$ErrorActionPreference = "Stop"
if (-not $Port) {
  $dev = Get-PnpDevice -Class Ports -PresentOnly |
         Where-Object { $_.InstanceId -match "VID_303A&PID_1001" } | Select-Object -First 1
  if (-not $dev) { throw "No 303A:1001 device found." }
  if ($dev.FriendlyName -match "\((COM\d+)\)") { $Port = $Matches[1] }
}
if (-not $Log) { $Log = Join-Path $PSScriptRoot ("pads-" + (Get-Date -Format "yyyyMMdd-HHmmss") + ".csv") }

$sp = New-Object System.IO.Ports.SerialPort $Port, 115200, "None", 8, "One"
$sp.DtrEnable = $true          # opening this port resets the S3 — expect the boot banner
$sp.ReadTimeout = 500
$sp.Open()
Write-Host "open $Port -> $Log   (Ctrl+C to stop)"
if ($Send) { Start-Sleep -Milliseconds 800; $sp.Write($Send + "`n"); Write-Host "sent: $Send" }

$deadline = if ($Seconds -gt 0) { (Get-Date).AddSeconds($Seconds) } else { [DateTime]::MaxValue }
$sw = [System.IO.StreamWriter]::new($Log)
try {
  while ((Get-Date) -lt $deadline) {
    try { $line = $sp.ReadLine() } catch [TimeoutException] { continue }
    $sw.WriteLine($line)
    if ($line -notlike "d *") { Write-Host $line }   # echo notices/rate, not every sample
  }
} finally { $sw.Close(); $sp.Close(); Write-Host "closed. log: $Log" }
