# ToastedDrums

A toy-keyboard drum machine for the laptop (ADRIAN-LT) that visualizes on the desk
annunciator (XIAO RP2040 + 43-px chain, panels 1/2 = the free 3×3s). Rust, zero crates.

Samples: [analogcode/toykeyboards](https://github.com/analogcode/toykeyboards)
(royalty-free hits from Yamaha PSS/PSR + Casio MT-52/MT-240) — cloned into `toykeyboards/`
(git-ignored; `git clone https://github.com/analogcode/toykeyboards` to restore).

## Shape
- **kit** (`kits/*.kit`): 9 voices, one per annunciator tile (slot → tile x=slot%3, y=slot/3),
  each with a colour and a hit WAV.
- **pattern** (`kits/*.pat`): 9 rows × 16 steps, `.`/`x`/`X`/`1-9` velocity, `bpm N`.
- `src/wav.rs` RIFF reader/writer · `src/kit.rs` · `src/seq.rs` sequencer+mixer+glow ·
  `src/vis.rs` 3×3 frame + COBS/CRC16 packet (byte-identical to `annunciator-rs/frame.rs`).

## Bench (runs on coffee0, no hardware)
```sh
cargo test
cargo build --release
./target/release/toasteddrums render kits/mt240.kit kits/basic.pat out/basic.wav 2   # offline mix
./target/release/toasteddrums show   kits/mt240.kit kits/basic.pat                   # per-step 3×3 frames
```

## Next (laptop build)
1. `live` command: WASAPI output (kernel FFI, house style) + paint panel 1 over COM9 at step rate.
2. Port ownership: COM9 is held by `annunciator.exe` — decide whether ToastedDrums is a mode of
   annunciator-rs or annunciator.exe exposes a local pipe (see `~/HANDOFF_HISTORY/ANNUNCIATOR_CLIENT_HANDOFF.md`).
3. Input: keyboard pads / MIDI.
