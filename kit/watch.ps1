# watch.ps1 - live viewer for the pads32 controller.
#   .\watch.ps1                            COM5 @ 115200, handshake, live bars + hits
#   .\watch.ps1 -Baud 921600               after `baud 921600` + `save`
#   .\watch.ps1 -Port COM6
#   .\watch.ps1 -Raw                       print firmware lines verbatim
#   .\watch.ps1 -NoGo                      stay in OFFER, watch the handshake itself
#   .\watch.ps1 -Send "set thresh p2 15"   send a command, then keep watching
# Ctrl+C to stop.
#
# DTR/RTS are left DEASSERTED deliberately: on a CP2102 devkit those lines drive EN and
# GPIO0 through the auto-reset transistors. Asserting them resets the board, and DTR alone
# holds GPIO0 low, which is exactly what drops it into "waiting for download".
param(
  [string]$Port = "COM5",
  [int]$Baud    = 115200,
  [switch]$Raw,
  [switch]$NoGo,
  [string]$Send = ""
)

$sp = New-Object System.IO.Ports.SerialPort $Port, $Baud, "None", 8, "One"
$sp.DtrEnable = $false; $sp.RtsEnable = $false
$sp.ReadTimeout = 1000
$sp.NewLine = "`n"
$sp.Open()
Write-Host "watching $Port @ $Baud - Ctrl+C to stop." -ForegroundColor Cyan

# The device sits in OFFER and streams nothing until a session exists, so introduce
# ourselves and start it. -NoGo leaves it offering, for watching the handshake.
if (-not $NoGo) {
  Start-Sleep -Milliseconds 200
  $sp.WriteLine("hello watch")
  Start-Sleep -Milliseconds 150
  $sp.WriteLine("go")
}
if ($Send) { Start-Sleep -Milliseconds 150; $sp.WriteLine($Send); Write-Host "sent: $Send" -ForegroundColor Yellow }

try {
  while ($true) {
    try { $line = $sp.ReadLine() } catch [TimeoutException] { continue }
    $line = $line.TrimEnd()
    if (-not $line) { continue }

    if ($Raw) { Write-Host $line; continue }

    switch -Regex ($line) {

      # a hit: pad name, velocity bar, and the numbers the detector actually used
      '^h (\d+) (\S+) (\d+) (\d+) ([\d.]+) base=(\d+) min=(\d+) slope=([\d.]+)' {
        $name  = $Matches[2]; $vel = [int]$Matches[3]
        $depth = [double]$Matches[5]; $slope = [double]$Matches[8]
        $bar   = '#' * [Math]::Max(1, [int]($vel / 4))
        # red means the velocity clipped at 127 - lower `gain` if you see a lot of it
        $col   = if ($vel -ge 127) { 'Red' } elseif ($vel -ge 90) { 'Yellow' } else { 'Green' }
        Write-Host ('{0,-4} vel {1,3} {2,-32} depth {3,5:N1}%  slope {4,5:N2}' -f `
                    $name, $vel, $bar, $depth, $slope) -ForegroundColor $col
      }

      # waveform behind a hit, squashed to a sparkline relative to that pad's baseline
      '^x (\d+) (\S+) base=(\d+) ms=1 v=(.+)$' {
        $name = $Matches[2]; $base = [double]$Matches[3]
        $vals = $Matches[4] -split ',' | ForEach-Object { [int]$_ }
        $spark = ($vals | ForEach-Object {
          $f = if ($base -gt 0) { $_ / $base } else { 0 }
          $i = [Math]::Min(7, [Math]::Max(0, [int]($f * 8)))
          [char](0x2581 + $i)          # lower-eighth block .. full block
        }) -join ''
        Write-Host ('  {0,-4} {1}' -f $name, $spark) -ForegroundColor DarkCyan
      }

      # raw window: min/mean/max per pad
      '^w ' {
        $out = foreach ($m in [regex]::Matches($line, '(\S+)=(\d+)/(\d+)/(\d+)')) {
          $mean = [int]$m.Groups[3].Value
          $bar  = '#' * [Math]::Min([int]($mean / 20), 24)
          '{0,-4}{1,4} {2,-24}' -f $m.Groups[1].Value, $mean, $bar
        }
        Write-Host ($out -join ' | ')
      }

      '^v '        { Write-Host $line -ForegroundColor DarkGray }
      '^cfg '      { Write-Host $line -ForegroundColor Magenta }
      '^(ok|pong)' { Write-Host $line -ForegroundColor DarkGreen }
      '^err'       { Write-Host $line -ForegroundColor Red }
      '^!'         { Write-Host $line -ForegroundColor DarkGray }
      default      { Write-Host $line }
    }
  }
} finally {
  try { $sp.Close() } catch { }
  Write-Host "`nclosed $Port"
}
