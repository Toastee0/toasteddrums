# ToastedDrums — pads bench kit for phobos-lt

Shipped from coffee0, 2026-08-31 ~23:50. For the Claude session running **on phobos-lt**.

## The goal
Build a real drum kit out of four of the operator's **custom capacitive pads** (stainless
steel sheet laminated to lexan, single crimped lead). His pads are sensitive enough that an
ESP32 touch channel reads **a foot through a thick shoe** — that is the design, not a hope.

The instrument is one board: **Seeed ReSpeaker Lite carrier with a factory-soldered
XIAO ESP32-S3**. It has both halves — hardware touch channels for the pads, and an audio
path out to the **SPK** JST through the XMOS + TLV320AIC3204 codec. So the pads, the
sequencer and the sound can all live on the one device.

The host-side half already exists on coffee0: `~/ToastedDrums` (Rust, zero crates) —
WAV reader, kit loader, 16-step sequencer, mixer, and a 3×3 visualizer that speaks the
desk annunciator's COBS/CRC16 packet format. It renders and mixes offline today.

## Hardware truth (do not re-derive — earned by the dead sonar project)
The ReSpeaker carrier claims these XIAO pins; the rest are free:

| XIAO pin | Carrier use |
|---|---|
| GPIO5 / GPIO6 | I2C — XMOS XU316 @ 0x42, TLV320AIC3204 codec @ 0x18 |
| GPIO7 / GPIO8 | I2S WS / BCLK (**the XMOS is I2S master**) |
| GPIO43 / GPIO44 | I2S TX (out to speaker) / RX (from mics) |
| **GPIO1, 2, 3, 4** | **free — TOUCH1..TOUCH4, one per pad** |
| GPIO9 | free — TOUCH9, spare fifth pad |

Live XMOS firmware is **I2S v1.0.8**, which keeps the I2C DFU servicer. **Do not reflash
the XMOS.** We do not need to: stock I2S firmware already carries audio from the XIAO to
the speaker. (The operator has an xTAG + clip tool if custom XMOS work ever resumes.)

## Wiring the pads — AS ACTUALLY SOLDERED (2026-09-01 ~00:00)
The operator soldered starting at **A1**, not A0, and was adding a further pin as this
shipped. So the pad→pin map is **not assumed**: the firmware scans every free touch-capable
channel — **GPIO 1, 2, 3, 4, 5, 9** — and prints them all. Touch one plate at a time and the
channel that moves names that pad's pin. Write the resulting map down here.

Known so far: A1=GPIO2, A2=GPIO3, A3=GPIO4 (TOUCH2-4). If a lead continued down past A3 it
is on **D4/GPIO5**, which is TOUCH5 and reads fine, but is also the XMOS **I2C SDA** — no
problem for the silent bench, but move that lead to the free A0/GPIO1 before wiring audio,
because the codec setup needs that bus.

The plate is the electrode; the return goes to the board GND. Long leads add baseline capacitance and pick up mains hum — expect the baseline to
differ per pad and per cable length. That is fine; it is why the firmware tracks a
per-pad baseline instead of using one global threshold.

## The firmware — `pads/pads.ino` (SILENT: no audio, numbers only)
esp32 core **3.3.11**, which uses the **NG touch driver**. Facts that cost time if unknown:
- On ESP32-S3 a touch reading **RISES** with capacitance (opposite of the classic ESP32).
- The touch FSM free-runs; `touchRead()` returns the driver's **SMOOTH** (IIR-filtered)
  value and does not block. If the smoothing turns out to blunt the strike attack, the fix
  is to drive `driver/touch_sens.h` directly and read `TOUCH_CHAN_DATA_TYPE_RAW`.
- `touchSetTiming()` / `touchSetConfig()` are latched at init and are **ignored once a pad
  is initialized**. So the firmware stores timing in RTC_NOINIT memory and reboots to apply
  — that is what `c` and `g` do. Not a bug.
- S3 defaults: 500 charge times, 0.5–2.2 V, 32 µs measure, 256 µs power-on wait
  → about 1.1 ms for a 4-pad scan (~900 Hz). Plenty for drums; a hit needs < 10 ms.

Serial, 115200 over the XIAO's native USB CDC:

| out | meaning |
|---|---|
| `d <us> <v...>` | one scan — one value per scanned channel, in `PADS[]` order (GPIO 1,2,3,4,5,9) |
| `r <scans_per_s> <us_per_scan>` | rate, once a second |
| `! <text>` | boot banner, config, baselines |

| in | meaning |
|---|---|
| `s` | stream on/off |
| `z` | re-zero baselines and print them |
| `c <measure_us> <sleep_us>` | set FSM timing, reboots to apply |
| `g <chg_times>` | set charge times (sensitivity/duration), reboots to apply |

## Session 1 — characterize, do not build yet
Flash, then get numbers before writing any trigger logic:
0. **Name the pins.** Touch each plate alone, note which of the six channels moves, and
   record the pad→GPIO map above. Everything downstream depends on it.
1. **Quiet baseline + noise** per pad, 10 s untouched. Note spread and any mains-rate ripple.
2. **Hit shape** — hand, then a foot in a shoe. Rise time, peak delta over baseline,
   how long it stays high, how it decays. This is what decides the trigger algorithm.
3. **Velocity** — is peak delta, or the slope of the rise, the better proxy for how hard
   it was struck? Capacitive pads have no force sensor; one of these two has to carry
   dynamics or the kit plays flat.
4. **Crosstalk** — hit pad 1, watch 2/3/4. Adjacent plates and bundled leads couple.
5. **The knee** — sweep `g` (charge times) and `c` (timing) for the point where
   sensitivity is still good but the scan is fastest. Latency is the thing a drummer feels.

## Rules
- **SILENT until the operator says otherwise.** It is nearly midnight; the kit shipped with
  no audio path enabled on purpose. Speaker tests need a verbal pass first (house rule:
  memory `ask-location-before-device-tests`).
- **COM3 is the desk annunciator** (XIAO RP2040, `annunciator.exe`, logon task). Do not open
  it, do not flash it. `flash.ps1` refuses COM3 outright. Our board is VID 303A PID 1001.
- Plug the **XIAO's own USB-C**, not the XMOS port beside the 3.5 mm jack.
- Rust/C for anything final; Python only for prototyping; **no JS/node** (house language policy).

## Files
| file | role |
|---|---|
| `pads.merged.bin` | prebuilt image from coffee0 — flash at 0x0, no toolchain needed |
| `flash.ps1` | auto-finds 303A:1001, installs esptool 4.8.1 via uv if missing, writes the image |
| `mon.ps1` | .NET SerialPort capture to CSV (`arduino-cli monitor` was flaky on the old laptop) |
| `bootstrap.ps1` | installs arduino-cli + **esp32 core 3.3.11 pinned** for a local build loop |
| `pads/pads.ino` | the firmware source |

Fastest start: `.\flash.ps1` then `.\mon.ps1`. Run `bootstrap.ps1` in another window
meanwhile — after it finishes, phobos can build the firmware itself and coffee0 is only
needed for the host-side Rust.
