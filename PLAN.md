# ToastedDrums — foley engine plan

Working document for the code agent. Decisions here are LOCKED unless the operator changes
them; everything under "Open" is not. Read `README.md` and `src/song.rs`'s header first.

This is a skunkworks project. There is no backwards-compatibility requirement: demo songs,
baked assets and goldens are build artifacts, and breaking a format is allowed when it buys
clarity or speed. Several constraints in earlier drafts existed only to protect compatibility
that is not wanted, and they have been removed rather than worked around.

## What this is

ToastedDrums grows a **procedural foley engine**: a node graph that turns a description of a
sound (a peeper chorus, a 60 Hz hum, a GSM burst, a shoe on wet concrete) into samples the
tracker plays. The same engine, built lean, links into the operator's games so they generate
their own audio at load.

The **goal** is that a game ships profiles rather than recordings — the profile is the asset
and the render stays on the workstation. It is a goal, not an invariant: `bake` already ships
a WAV for every slot that is not a wub (`src/main.rs`; a wub bakes as parameters instead,
which is the goal working), and a Phase 2 slice profile's truth *is* a WAV.
Both are legitimate. What the engine guarantees is that a runtime profile is **loadable and
affordable**, not that it is tasteful — see decision 3.

Two consumers, one crate, two build profiles:

| | studio (tracker) | runtime (game) |
|---|---|---|
| deps | whatever it needs | zero |
| allocation | free | none after `init` outside the GPU driver; arena; post-init allocation aborts in tests |
| node catalog | everything | core set only (feature-gated out, not disabled) |
| scripting | Rhai tiers 1-2 | none |
| I/O | zip/.tdfx, WAV cache, files | JSON + CSV + blobs handed in by the caller |
| parallel | realtime: single-threaded. export: Rayon across populations | single-threaded, except inside the optional GPU driver |
| budget | none — let the workstation fall over | declared per profile, checked at save, enforced at `bake`, clamped at runtime |
| GPU | never in the render path | optional, feature-gated, off by default, CPU fallback mandatory |

## Locked decisions

1. **Project rate.** `song.json` carries `"rate"` (default 48000). Everything mixes at the
   project rate. Samples resample ONCE at load (windowed sinc; `Kit::load(path, rate)`), and
   resampling to the rate a sample is already in is a **strict identity** — no filter, no
   interpolation — which is what lets a 44.1 kHz project keep producing the bytes it always
   did. Profiles render AT the project rate; rate is a term in the render hash.
   - The **engine** (`mcp`/`ui`) uses the audio device's native rate as its project rate, so
     `song::Resampler` is not constructed at all on the normal path. It survives only for a
     device that will not open at the project rate.
   - A song loaded into a running engine **adopts** the engine's rate and says so
     (`Studio::adopt_rate`), because the engine mixes at one rate fixed when the device opened.
   - Bench commands (`render`, `show`, `live`) have no song to carry a rate and use
     `BENCH_RATE` (44100), which is what they always produced.
   - `bake` takes its own target rate — the game's rate is independent of the project's.
2. **Two modes, one signal path.** The engine runs the same graph two ways:
   - **Realtime** — end-to-end stereo, allocation-free, deadline-bound. Capture, monitoring,
     and at most ONE live foley graph instrument. What is rationed here is graph instances,
     not kit voices: a full nine-voice song plays in realtime today and goes on doing so.
   - **Export** — the same signal path with no realtime deadline: heavily buffered,
     step-processed, parallel across independent work, for rendering a full layered song.

   The invariant is that for the same inputs, the same `build_id` and the CPU backend, the two
   modes agree **bit for bit**. That makes block-size independence testable rather than hoped
   for.
   The graph plugs in as a `kit::Source` kind, so it reaches the mixer through the seam that
   already exists rather than through a second code path.
3. **Profile = the unit, and lint checks feasibility, not taste.** `.tdfx` is a zip of
   `profile.json` (graph, params, seed, target, budget, hashes, version) + optional
   `data.csv` (bulk numeric input: spectra, burst patterns, curves, tables — typed header
   line) + optional `*.wav` (samples the graph references, and/or a cached render).

   A runtime profile **may** reference WAVs. Policing what a game imports is the game's job,
   not this engine's. Lint refuses only what would break the runtime contract:
   - a studio-only node,
   - state whose size is not fixed at `init`,
   - a budget overrun — the fields are `max_voices`, `max_len_s`, `max_mem` and
     `max_render_ms`, and `max_mem` counts WAV payload bytes, since that is real memory,
   - a breach of those parts of decision 4's forbidden list that are properties of a
     **profile** rather than of the engine build — RNG source, wall clock, host and filesystem
     state, hash-map iteration, scheduling-dependent reduction. ("No fast-math" is a property
     of how `foley` itself is compiled and is enforced there, not by profile lint.)
   - anything the C ABI cannot load.

   It never refuses on authorial grounds. The rule is: don't make it impossible to *create*
   the runtime; do not try to prevent someone doing something deliberate with it.
4. **Determinism, in tiers.** The old claim — bit-identical on every machine, no escape
   hatch — was false and is retired. `src/wub.rs` alone makes five libm calls per sample
   (`sin`/`cos`/`powf`/`sin`/`tanh`) feeding a recursive Chamberlin filter, so last-ulp libm
   differences **compound** rather than stay bounded. The C port already concedes this: its
   golden compares with a tolerance (`Sand walker/tests/test_tracker.c`), and that project's
   `AUDIO.md` records that the observed 0.0 diff happens only because Rust windows-gnu and
   GCC both link mingw-w64 libm.

   | tier | scope | assertion |
   |---|---|---|
   | T1 | same `build_id`, CPU backend | bit-identical, always |
   | T2 | across target platforms, CPU backend | tolerance: max-abs 2e-3, RMS 1e-4 |
   | T3 | GPU backend, any platform | tolerance only, never a golden |

   The backend is a term in `build_id` (decision 10), so a GPU build has its own identity;
   T1 is scoped to CPU precisely so it does not promise what T3 denies.

   What determinism still **forbids** — all of it implementable as a lint, unlike "a
   non-deterministic node", which under T2 would reject the entire Phase 1 catalogue:
   host-supplied seeded RNG only, no wall clock, no filesystem or host state, no hash-map
   iteration anywhere a script or node can see, no reduction whose order depends on thread
   scheduling, no state whose size is not fixed at `init`. No fast-math.
5. **One implementation.** `foley` builds as `staticlib` + `cdylib` with a C ABI (`foley.h`)
   and the game links it. **No C port of nodes**, ever — that rule is the point of the crate.
   The existing C mixer keeps playing kick/wub/string, but it is not frozen: when the baked
   format changes, the C loader and its golden are regenerated with it.
6. **Three rate domains in the graph**, distinct in the model and the UI:
   *event* (stochastic processes emitting timestamped param payloads), *audio* (per-event
   synthesis), *shared tail* (one ER network + reverb every voice converges on). Population
   nodes fan out a subgraph N times with per-instance parameter distributions; coupling
   (Kuramoto-style, weak) is a node that operates ACROSS a population, not inside a voice.
7. **Per-hit overrides generalise.** `Mods` keeps its 9 typed fields (legacy voices, C port).
   A hit may additionally carry `params: BTreeMap<String, f32>` keyed by `node.param` path;
   those are a term in the **cache key** (decision 10). A hit that sets graph params on a
   runtime-targeted profile is baked as its own variant, never as a runtime mod.
8. **House style holds, with two named carve-outs.** Zero crates in the runtime build.
   Pinned, registry-only deps in the studio build, vetted the way cpal/egui were.
   Doc-comment headers that say WHY. The runtime envelope — no allocation after `init`,
   `#![deny(unsafe_code)]`, `panic = "abort"`, single-threaded — has exactly two exceptions:
   the ABI shim, and the optional GPU backend. That backend is feature-gated, **off by
   default**, and must always have a working CPU fallback; it allocates and spawns threads
   inside the driver, so it sits outside both the alloc guard and the single-threaded rule.

   CI **must** build and test `--no-default-features`. `default = ["studio"]` otherwise
   guarantees the zero-dependency build rots undetected, since nothing else ever builds it
   and feature unification can re-enable `studio` through any dependent.
9. **Targets.** Windows 10/11 and Steam Deck. macOS is out of scope and no effort is spent on
   it. Where something is not portable, it is adjusted per target rather than abstracted.

   The shipped staticlib pins `x86_64-pc-windows-gnu` on Windows. That pin is load-bearing,
   not incidental: it is the only reason the Rust engine and the C game agree as closely as
   they do, because both then link the same mingw-w64 libm. The Deck is
   `x86_64-unknown-linux-gnu` and agrees only to T2.
10. **Identity is not the cache key.** Because determinism is per-platform (decision 4), the
    render hash alone can no longer key a cache: the same hash yields different bytes on
    different platforms, and a toolchain upgrade changes the bytes with no version bump —
    which would silently serve a stale render on the *same* machine.

    - `render_hash` = profile **identity**: an ordered, versioned list of terms — graph,
      params, `data.csv`, seed, rate, engine_version, and (from Phase 4) script source hash.
      Appending a term bumps the list version explicitly; nothing is ever added silently.
      Used for naming and the ABI. A golden is stored per `render_hash` × `build_id`: the
      same argument that disqualifies `render_hash` as a cache key disqualifies it as a golden
      key, since a toolchain upgrade changes the bytes just as a platform change does. The
      hash names the golden; the `build_id` selects which one to compare against.
    - `build_id` = `H(target triple ∥ toolchain ∥ backend ∥ engine_version)`.
    - `cache_key` = `render_hash ∥ build_id ∥ hit params`.

    The WAV cache is never committed and never shared between machines.

## Phases

Each phase is shippable on its own. Do not start a phase before the previous one's acceptance
passes. Keep `cargo test` green. Baked assets, demo songs and goldens are regenerable build
artifacts: a golden that changes is a question to answer, never a veto on the change.

### Phase 0 — seams (no new features) — **DONE**

Implemented and verified. What landed:

- `kit::Source { Sample { path }, Kick, Wub, String }` with `build` (which owns the whole
  mutation key table), `render_baked(rate, dir)` and `takes_mutations()`. Adding a fifth kind
  is a variant plus two match arms; `Kit::load`'s voice arm went from ~60 lines with two
  special-cased `continue`s to six.
- `Kit::load(path, rate)` and `Song.rate`. Samples resample at load; the "first sample decides
  the kit rate" rule and the "kit rate differs from the running" refusal are both gone.
- `song::Instance` + `Playing { Buffer, Wub, String }`, replacing the old `Option<WubState>`
  plus `is_string` pair — `mix` is now one match with three arms.
- Audio-thread allocations fixed: a reused scratch in `trigger_step`, committed capacity in
  `Resampler`, a pre-sized mono buffer in `audio.rs`.
- `src/noalloc.rs`: a debug-build `GlobalAlloc` wrapper with a const-init thread-local flag
  that counts (never panics or prints — the panic machinery allocates) any allocation inside
  the audio callback. Release compiles it to nothing.

Verified: 46 tests pass. `bake` reproduced the September golden byte-for-byte —
`golden.wav`, all eight slot WAVs and `song.txt` identical, `song.json` differing by exactly
the one `"rate": 44100` line. A 48 kHz bake of the same song gives 318000 frames against
292116, a ratio of 1.0886 versus the expected 1.0884: real resampling, same 6.62 s of music.

### Phase 0b — cut the voice-buffer seam once

Phase 0 gave each voice a single mono `Vec<f32>`. Phase 1 needs variants and Phase 3 wanted
stereo, so that seam was going to be re-cut twice. Cut it once instead, now, before the graph
lands on top of it.

- `Voice`'s buffer becomes **variant-indexed and channel-interleaved**. `mix` reads it with a
  channel stride; `Transport::start` picks a variant round-robin (or seeded).
- The Transport goes **stereo end-to-end** (decision 2). `fill` produces interleaved L/R and
  `audio.rs` maps channels instead of duplicating mono. Kick/wub/string stay mono sources
  panned centre.
- `bake`'s format changes to carry variants and channels. The C loader and its golden are
  regenerated in the same pass (decision 5).

Accept: tests green; the two-mode equivalence test passes — the same song rendered through
small blocks and through large blocks is bit-identical; a stereo voice reaches both speakers
and a mono one is centred; the alloc guard still reports nothing during a long `play`.

### Phase 1 — `foley` crate, offline, studio + runtime builds

Workspace: `crates/foley/` (`[features] default = ["studio"]`), root binary depends on it
with `studio`.

- `Graph`: nodes, typed ports (event / audio / control), DAG validated and topo-sorted at load
  into a flat instruction list with preallocated buffer indices. Stereo throughout — the kit
  seam is stereo as of 0b, so nothing is downmixed on the way in.
- Node trait: `prepare(&mut self, ctx)`, `process(&mut self, in, out, ctx)`,
  `params() -> &[ParamSpec]` (name, range, unit, default, curve — this generates the UI and
  the CSV column schema; get it right early). Studio-only nodes behind
  `#[cfg(feature = "studio")]`.
- Core (runtime) catalog, in this order of value:
  - excitation: impulse, shaped noise burst, pulse train, **stick-slip oscillator**
    (relaxation osc with a friction curve — this one node is cork-on-glass, branch creak,
    shoe drag, hinge, bow, brake squeal), granular scatter
  - resonator: modal bank (freq/amp/decay triplets), formant/biquad chain, waveguide
    (tube/string/bottle), comb+allpass
  - shaping: AR/ADSR/breakpoint envelopes, LFO, random walk, 1/f drift, saturation,
    wavefolder, **rattle/buzz** (contact nonlinearity)
  - event: Poisson spawner, refractory gate, Neyman-Scott cluster, Markov state, weak
    coupling, **population** (fan-out with per-instance distributions + voice allocation)
  - spatial: shared ER network with per-voice random delay, distance model (LPF+gain+wet/dry
    as one param), Doppler pan
  - noise injection: mains hum (f0, harmonic profile, ±0.05 Hz drift, ground-loop asymmetry),
    RF demod (burst rate, frame pattern, envelope, pickup resonance — GSM = 217 Hz bursts,
    4.615 ms frame; also covers switching supplies), quantise/dither, tape/vinyl (wow,
    flutter, surface, dropouts), crosstalk (sidechain through a nasty filter), impulse noise.
    Every injector has two attachment modes: *additive* and *signal-dependent* (sidechain from
    host signal drives level/filter — AGC pumping is what makes a bad phone recording read as
    one).
- Studio-only: long convolution, and anything whose **state size is not fixed at `init`**.
  Note the distinction: the shared ER + reverb tail is long, not unbounded, and its delay
  lines are sized once — so it is legal at runtime, which the old "unbounded state" wording
  accidentally outlawed.
- **Voice count is clamped at runtime**, hard. A Poisson spawner's population cannot be
  bounded by a save-time check, so `max_voices` is enforced where voices are allocated.
- `Profile` loader: `.tdfx` zip — the **stored-only zip reader is studio-only** (hand-rolled,
  no crate); the runtime takes JSON + CSV + blobs handed in by the caller and never parses a
  container. Stored-only means a WAV-bearing profile does not compress; accepted.
- C ABI: `foley_init`, `foley_load(json, json_len, csv, csv_len, blobs, blob_count) -> handle`,
  `foley_render(handle, seed, rate, out, out_len) -> written`, `foley_free`. The blob
  parameter is required, not optional: without it a Phase 2 slice profile cannot load at all.
  The runtime therefore needs a zero-dependency WAV decoder — share `src/wav.rs` into the
  crate rather than writing a second one. `foley.h` generated and checked in.
- Runtime hygiene: `foley_init(arena_bytes)`; every allocation through the arena; post-init
  allocation aborts under a test allocator.
- Integration: kit line `voice 3 peepers 0,200,255 profiles/peepers.tdfx seed=4`.
  `Kit::load` → `cache_key` → hit ? load : render → the voice's variant buffers.
- CLI: `foley render x.tdfx out.wav [seed=N] [rate=N]` (rate defaults to the profile's own,
  and is a `render_hash` term, so it is never implicit), `foley lint x.tdfx`, `foley hash x.tdfx`,
  `foley bench x.tdfx` (produces the per-platform timing stamp that `max_render_ms` is checked
  against — it cannot be measured on a Windows workstation, so lint checks that a stamp for
  the target exists and fits, never that it re-measures). `tools/deck_bench.py` runs
  `foley bench` on the Deck and records that stamp. The MCP door gains `foley_lint`.
- Tests: per-profile goldens **per tier** — T1 exact within a `build_id` on the CPU backend,
  T2 against the tolerance; a determinism test rendering twice on two threads and comparing
  bit-for-bit; a
  runtime alloc test; lint fixtures, one passing and one per rejection reason.

Accept: a `peepers.tdfx` (population of FM chirps, Poisson, refractory, weak coupling, shared
ER) and a `hum60.tdfx` render, lint as runtime, and play from a kit slot. The tracker and a C
test program linking `libfoley.a` agree — exactly on the same `build_id` and CPU backend,
within tolerance across platforms. `cargo test --no-default-features` builds and passes in CI.

### Phase 2 — audio capture

- cpal input stream → SPSC ring (hand-rolled, `AtomicUsize` head/tail) → writer thread → WAV.
  Every buffer stamped with `global_step` + `pos_in_step` at buffer start, written to a
  sidecar `.take.json` so the take is on the pattern clock. 256–512 frame buffers are fine; a
  glitched take is a retake.
- Door verbs: `record_audio {on, slot?}`, `takes_list`, `take_slice {take, method}`.
- Offline slicing: onset detect (spectral flux), snap to grid using the sidecar, trim,
  normalise, each slice wrapped as a `.tdfx` whose graph is `sample → chain`. Slices ride
  Phase 1's cache/variant/lint machinery unchanged.
- Capture is studio-only. The resulting slice profiles are **not** refused a runtime target
  (decision 3) — they just have to fit the budget like anything else.

Accept: record a pad take over a playing pattern, slice it, load the slices into a kit, play
them. Grid offsets shown per slice.

### Phase 3 — generalised mods, bus chain

- `Hit.params: BTreeMap<String, f32>`; it joins the cache key; bake renders a distinct variant
  per unique param set on a runtime profile.
- Bus-level insert chain (`song.json: "bus": [profile refs]`) so a whole song can be declared
  as "through a 1994 answering machine", with per-track opt-out. This belongs to the **export**
  mode, not the realtime one — decision 2 rations live graph instances to one, and a bus chain
  is a graph across every track, so putting it in the realtime path would quietly break that.

(Stereo moved to Phase 0b.)

### Phase 4 — human scripting

- Rhai, studio-only, tier 1 (graph construction at load) and tier 2 (control rate on the
  non-audio thread, allocation-free in steady state). No tier 3. API: build graph, declare
  params, read CSV, host RNG, control-frame index + musical position. No FS, network, threads,
  clock. The script source hash is an explicit appended term in `render_hash` (decision 10),
  which bumps the term-list version. Hot reload: rebuild graph, migrate params by name, atomic
  swap.
- The MCP door already serves as tier 1 for an LLM; this is for people.

## Touch points (current tree)

- `src/kit.rs` — `Source`, `Kit::load(path, rate)`, `resample`, profile source kind, variants
- `src/song.rs` — `Song.rate`, `Hit.params`, `Instance`/`Playing`, variant pick in `start`,
  stereo `fill` in 0b, `Resampler` demoted to fallback
- `src/audio.rs` — channel map in 0b; input stream in Phase 2
- `src/noalloc.rs` — the tracker binary's debug-only audio-thread allocation counter. It is
  NOT `foley`'s arena guard, which is a separate runtime-side thing built in Phase 1; the two
  are easy to confuse and answer to different rules
- `src/mcp.rs` — new door verbs, `foley_lint` among them (Phase 1)
- `src/main.rs` — `foley` subcommands; `bake` target rate and format
- `src/wav.rs` — shared into `foley` for the runtime's blob decoding
- `src/wub.rs`, `src/string.rs` — the per-sample libm callers, i.e. exactly the code decision 9
  means by "adjusted per target"; not untouchable
- `tools/` — keep `analyze_track.py`; add `deck_bench.py` for the Deck's `foley bench` stamp
- new: `crates/foley/`, `profiles/`, `foley.h`

## Open

- Whether `profiles/` sits beside `kits/` (proposed: yes — one chorus serves many kits) or
  inside a kit directory. Note the render cache is keyed by `build_id` and is never committed,
  so it does not live there.
- `max_len_s` default for runtime profiles (proposed 30 s; a 30 s stereo 48k bed is 11 MB and
  the Deck shares 16 GB with the game).
- The GPU backend's shape. It is in the contract as an optional, off-by-default,
  CPU-fallback-mandatory backend (decision 8) and may stay a stub for a long time — every
  target has a GPU, including the Deck, so it is worth exploring on its own merits rather than
  only as a rescue for a slow loading bar. Two things to settle before any code: the ABI needs
  a way to accept the **game's existing** Vulkan device and queue, since creating one during
  the loading bar contends with the game's own load work; and T3 output is tolerance-tested,
  never a golden.
- Whether the Sand Walker C mixer is eventually replaced by linking the Rust engine too.
  Not this plan.
