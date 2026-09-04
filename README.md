# ToastedDrums

A toy-keyboard drum machine that visualizes on the desk annunciator (XIAO RP2040 + 43-px
chain, panels 1/2 = the free 3×3s), played from four capacitive steel pads. Rust, zero crates.

Samples: [analogcode/toykeyboards](https://github.com/analogcode/toykeyboards)
(royalty-free hits from Yamaha PSS/PSR + Casio MT-52/MT-240) — cloned into `toykeyboards/`
(git-ignored; `git clone https://github.com/analogcode/toykeyboards` to restore).

## Shape
- **kit** (`kits/*.kit`): 9 voices, one per annunciator tile (slot → tile x=slot%3, y=slot/3),
  each with a colour and a hit WAV.
- **pattern** (`kits/*.pat`): 9 rows × 16 steps, `.`/`x`/`X`/`1-9` velocity, `bpm N`.
- `src/wav.rs` RIFF reader/writer · `src/kit.rs` · `src/seq.rs` sequencer+mixer+glow ·
  `src/vis.rs` 3×3 frame + COBS/CRC16 packet (byte-identical to `annunciator-rs/frame.rs`).

## Bench (no hardware)
```sh
cargo test
cargo build --release
./target/release/toasteddrums render kits/mt240.kit kits/basic.pat out/basic.wav 2   # offline mix
./target/release/toasteddrums show   kits/mt240.kit kits/basic.pat                   # per-step 3×3 frames
```

## The pads — `fw/pads32` (built and working on PHOBOS-LT)

Four bare steel plates: two screwed to the wall (hand), two lying on carpet (foot). They
drive a **classic ESP32 devkit** (ESP32-WROOM-32, CP2102 → COM5) on **GPIO 2, 4, 33, 32**.
The board is an INPUT DEVICE ONLY — all sound is made by the host.

Build: `arduino-cli compile -b esp32:esp32:esp32 ./fw/pads32` (bare FQBN — see kit doc).
Watch: `.\kit\watch.ps1` — does the handshake and draws hits, velocities and waveforms.

### Why not the ReSpeaker/XIAO-S3 in `kit/`
The original plan (`fw/pads`, `kit/`) targeted a Seeed ReSpeaker Lite + XIAO ESP32-S3, for
its onboard codec. Two things killed it, both recorded in `kit/PHOBOS_KIT.md`:
1. The carrier only breaks out **two** usable touch pins. GPIO16/17 have no touch hardware
   at all, and SDA/SCL carry the codec's I²C pull-ups.
2. The ESP32-S3 NG touch driver **hangs** — `touchBenchmarkThreshold()` burns 3 × 2000 ms
   `oneshot_scanning` timeouts per channel and never recovers.

Since the host makes the sound, the codec was worth nothing, and the classic ESP32 has ten
touch channels and no such hang. `kit/` is kept for the S3 history and the trap list.

### Serial protocol (115200 8N1 default, `baud <n>` to go faster)
The device calibrates for 1 s at boot (median of 256 samples/pad — **do not touch the pads**),
then sits in **OFFER**, beaconing its normals and streaming nothing until a session exists:

```
! offer pads32 proto=5 pads=4 P2=145 P4=138 P33=502 P32=476
hello drumkit     → ok hello ... + the current cfg blob (populate your sliders from this)
cfg <blob>        → ok cfg applied=N rejected=M
go                → LIVE
```

If no host ever speaks it goes LIVE on saved NVS config after 8 s, so a plain terminal
works. Hits are always queued, so either style works:

```
h 0 P2 45 12345 22.1 base=145 min=113 slope=5.21     pushed
poll → ok poll n=2 ms=12456 dropped=0 + h lines + v P2=145,144 ...   pulled
```

`thresh` / `slope` / `gain` are **per-pad and live** — no reboot — so the drum machine can
have real sensitivity sliders. Only `dur` needs a restart (`touchSetConfig()` latches before
the first `touchRead()`). Config persists in NVS via `save`.

### Facts that cost time (full list in `kit/PHOBOS_KIT.md`)
- Values **fall** when touched on this chip. Hits are 60–70% deflections.
- Baselines differ hugely by mounting (wall ~500–700, floor on carpet ~140–450), and ambient
  drift is the same size as a hit — so every threshold is a **percentage of that pad's own
  tracked baseline**, never an absolute count.
- A shoe sole is a thick dielectric; a hand on bare metal is nearly direct contact. Foot pads
  couple far more weakly for equal effort. Hence per-pad tuning.
- Never use GPIO0 as a pad: it is the BOOT strap, its pull-up clamps it to a flat 0, and the
  CP2102's DTR line drives it — a terminal asserting DTR drops the chip into
  "waiting for download". `watch.ps1` deasserts DTR **and** RTS for this reason.

## Building on Windows
Rust on the **GNU** toolchain (`stable-x86_64-pc-windows-gnu`; there is no MSVC C++ workload
on phobos). The one crate, `cpal`, pulls in Microsoft's `windows-*` bindings, and those need
`dlltool` to build import libraries — rustup's bundled copy fails to spawn, so a real
mingw-w64 is required: `winget install BrechtSanders.WinLibs.POSIX.UCRT`, which puts
`mingw64\bin` on the user PATH. Open a new shell after installing it.

**Audio is `cpal`, pinned `=0.18.2`, default features.** It replaced a hand-rolled `waveOut`
after a pure sine measured an underrun every fourth buffer regardless of pacing: waveOut is a
shim over the WASAPI shared-mode engine and drains its whole queue once per ~10 ms period,
so it cannot do sub-period latency. cpal opens the device at its **native rate (48 kHz here)**
and the mixer pitch-corrects the 44.1 kHz kit with a fractional cursor — asking the device for
the kit's rate is what made everything play flat and slow. Vetted before adding: RustAudio
repo, every dependency from the crates.io registry, no git/path sources.

### Calibration is per session, per striker
`live` never carries thresholds between runs. On start it measures the untouched noise floor
(1 s), then asks for **three taps per pad** and derives that pad's `thresh` (half a typical
hit, never inside the noise floor) and `gain` (1.6× a typical hit, so normal playing lands
near vel 80). Tap the foot pads with your feet and the hand pads with your hands or sticks:
the kit learns *that* strike on *that* mounting, and a hand on a foot-tuned pad will
overdrive it — which is the sign it's measuring something real. A pad left untapped keeps
its learn threshold and reports uncalibrated velocity rather than inventing a gain.

## Next
1. Learn-phase UX: a way to redo one pad mid-session without restarting, and a keypress to
   skip the taps when you just want the last numbers back.
2. Paint panel 1 over COM9 at hit time (the `vis.rs` frames already exist).
3. Port ownership: COM9 is held by `annunciator.exe` — decide whether ToastedDrums is a mode of
   annunciator-rs or annunciator.exe exposes a local pipe (see `~/HANDOFF_HISTORY/ANNUNCIATOR_CLIENT_HANDOFF.md`).
4. Map 4 pads onto 9 voices (bank/shift?), and decide whether pads play live over the
   sequencer or record into the pattern.
5. Velocity curve: `gain` is linear and the level is `(vel/127)²`. Real kits want a shaped
   curve, and it belongs on the host where it can be changed without reflashing.
6. Lower latency still: cpal WASAPI **exclusive** mode gets under the 10 ms shared-mode period.
