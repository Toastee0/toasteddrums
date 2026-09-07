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
  drift is the same size as a hit — so thresholds are **absolute counts, per pad, learned from three taps each session**
  (percentages were tried and removed -- see PROTOCOL.md 6.1).
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

## The tracker: one engine, two doors

`toasteddrums mcp [kit] [pads-port] [door-port]` runs the engine as a long-lived process that
owns the audio device, the pads and the song, and exposes the same verbs two ways:

- **MCP over stdio** (`.mcp.json` registers it for Claude Code) — so Claude can write patterns,
  swap kits, arm modifiers, record from the pads and render, in the conversation.
- **A localhost TCP door** (`127.0.0.1:<door-port>`, same newline-delimited JSON-RPC, same tool
  names) — for the operator UI. Both edit ONE song; there are never two copies.

The song model (`src/song.rs`) is JSON end to end: a BPM and any number of **tracks**, each
with its own length in sixteenths (16 = 4/4, 12 = 3/4, 14 = 7/8 …) looping independently, so
polymeter is the default. Each cell holds hits, and each hit carries its own **mods** — pitch,
drive, crush, reverse, gain, decay — applied at mix time for *that* hit only. That is the
"any modifier, any note" rule. Tracks carry a pad map (which slot each plate plays while the
track is the context) and a MIDI key map.

Tools: `status song_get song_set song_save song_load track_add track_set track_clear hit_set
hit_add fill mods_apply play stop bpm master trigger kit_load kit_info render pads_open
pads_learn pads_context record arm midi_list midi_open midi_context`. `tools/list` carries the schemas. Verified over stdio:
12/12 replies, nothing but protocol on stdout, `bpm` changes mid-play without a glitch.

The pads session (`hello`/`go`/`cal`) and the three-tap learn live once, in `src/pads.rs`
(`session_start`, `calibrate`, `Learn`), and `live` and `mcp` both use them.

## The operator UI: a tracker inside an 808

```
toasteddrums ui                    # attaches to mcp's door on 127.0.0.1:4242, or runs the engine itself
toasteddrums ui 4242 kits/mt240.kit COM5   # port / kit / pads, any order
```

`src/ui.rs` is eframe/egui (**pinned `=0.36.1`, default features off + `glow` +
`default_fonts`**; 0.36's default backend is wgpu, which is a far larger tree). Vetted the
same way as cpal: emilk/egui, every one of the 133 packages that build on windows-gnu comes
from the crates.io registry, no git or path sources.

The UI is a **client of the door**. It never holds the song: it speaks the same JSON-RPC
verbs Claude does, over TCP, so the two of you edit ONE song. With nothing listening on the
port it starts the engine in-process and connects to that -- same code path, no Claude
required. `.mcp.json` passes `4242` so Claude's server always opens the door.

Layout: rows are steps and columns the nine kit slots, tracker-style; a cell reads
`VV FFFFFF` -- velocity in hex, then P D C R G L flags for pitch / drive / crush / reverse /
gain / decay, so "any modifier, any note" is visible per cell. Around the grid, the 808:
the instrument strip (tap buttons, TUNE / LEVEL / DECAY per voice applied to every hit of
that voice in the pattern), the sixteen step buttons in red / orange / yellow / white with
the running light for the selected voice, START / STOP, TEMPO, MASTER, REC and ACCENT.
Patterns on the left are the tracks: each has its own length, so 4/4 against 3/4 is two
patterns. The right panel edits the hit under the cursor, the armed mods for the pads and
the keys, opens the pads (with LEARN) and the MIDI keyboard.

Keys, when no text field has focus:
```
arrows / PgUp PgDn / Home End   cursor        Tab, Shift+Tab   next / prev pattern
1-9   write a hit at that ninth (9 = accent)  0 Del Backspace  clear the cell
Enter toggle a hit (x, or X with ACCENT lit)  a                accent on / off
r     reverse on / off                        - =              pitch down / up a semitone
z x c v b n m , .   tap slots 0-8 -- records while REC and playing, like TAP WRITE
Space START / STOP
```

Measured on phobos (1536×864 logical at 125 %): the window opens maximised and everything
fits without the grid scrolling sideways. Edits round-trip through the door in well under a
frame; the UI applies each edit to its own snapshot at once and a poll that raced with a
queued edit is dropped, so nothing flickers.

## Next
1. Verify MIDI on the Casio. `src/midi.rs` (winmm, zero crates) is built and unit-tested --
   drum mode maps notes to slots through the track key map, chromatic mode pitches one slot
   by note, and the keys have their own context and armed mods -- but it has never met the
   keyboard, which does not fit in the shed. Check: `toasteddrums midi`, then `midi_open`
   and `midi_context` over MCP, or the KEYS panel in the UI.
2. Play the UI with the pads on the bench: open COM5 from the PADS panel, learn, REC, tap.
3. Paint panel 1 over COM9 at hit time (the `vis.rs` frames already exist).
4. Lower latency still: cpal WASAPI **exclusive** mode gets under the 10 ms shared-mode period.
