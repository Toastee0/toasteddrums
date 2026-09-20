//! A kit = up to 9 voices (one per annunciator tile, 3×3) each backed by one toykeyboards hit.
//! Kit file format (plain lines, `#` comments):
//!   name <kit name>
//!   voice <slot 0-8> <label> <r,g,b> <source> [mutations...]
//! Paths are relative to the kit file's directory.
//!
//! A kit is loaded FOR a project rate (`Kit::load(path, rate)`) and every voice ends up
//! rendered at it: synths generate there directly, and a sample recorded at another rate is
//! resampled once, here, so the mixer never asks what rate a voice is in. A sample already
//! at the project rate is passed through untouched — that exactness is what lets a 44.1 kHz
//! project keep producing the bytes it always did. There is no "the first sample decides the
//! kit's rate" rule any more, and a kit may freely mix sample rates.
//!
//! <source> is a WAV path, `synth:kick`, `synth:wub`, or `synth:string`. Mutations are
//! trailing tokens, applied in the order listed below and BAKED into the voice at load —
//! zero cost at play time (a wub or string is generated per hit instead; see wub.rs / below):
//!   pitch=<ratio>  playback speed: 0.5 = an octave down, 2.0 = up. Resampled once here.
//!   rev            reverse
//!   crush=<bits>   quantise to 2^bits levels: 8 is gritty, 4 is destroyed
//!   drive=<x>      tanh saturation, x = how hard: 1 is gentle, 4 is a wall, 10 is fuzz
//!   gain=<x>       linear level, after everything else
//!
//! synth:kick parameters (all optional):
//!   f0=<hz>        where the pitch drop lands and sits              (default 50)
//!   sweep=<hz>     where it starts                                  (default 120)
//!   decay=<ms>     amplitude decay to -60 dB                        (default 400)
//!   click=<0-1>    a few ms of noise on the attack, for the beater  (default 0.3)
//!
//! synth:string parameters (a plucked string -- see string.rs -- all optional):
//!   note=<0-127>   the MIDI note a hit plays when it names none      (default 28, E1)
//!   pick=<.02-.5>  pluck position along the string, 0.12 near the bridge  (default 0.12)
//!   tone=<0-1>     pick hardness: 1 a plectrum, 0.2 a thumb          (default 0.55)
//!   damp=<0-0.9>   how fast the highs die; high is a palm mute       (default 0.3)
//!   decay=<ms>     time to -60 dB                                    (default 1600)
//!   lp=<hz>        output lowpass                                    (default 4500)
//!
//! This is the whole "make it sound like Prodigy instead of a music class" mechanism:
//! those records are samples mutated hard and looped at the right cadence. Two references
//! measured (tools/analyze_track.py): half of everything sits below 120 Hz and hats are
//! under 1%, so the sub kick is a sine with a pitch drop -- a sample cannot give you that,
//! but an 808 is exactly a sine -- and the toy-keyboard hits get pitched down and driven.

use crate::string;
use crate::wav::Wav;
use std::path::{Path, PathBuf};
use crate::wub::{self, WubParams};

/// Parsed `key=value` / flag mutations from the end of a voice line.
#[derive(Default)]
struct Muts {
    pitch: Option<f32>,
    rev: bool,
    crush: Option<u32>,
    drive: Option<f32>,
    gain: Option<f32>,
    /// synth parameters, kept generic so new synth kinds need no new fields
    f0: Option<f32>,
    sweep: Option<f32>,
    decay: Option<f32>,
    click: Option<f32>,
    /// wub parameters
    wave: Option<u8>,
    detune: Option<f32>,
    sub: Option<f32>,
    cutoff: Option<f32>,
    floor: Option<f32>,
    res: Option<f32>,
    wob: Option<f32>,
    hold: Option<f32>,
    /// synth:string parameters (see string.rs)
    note: Option<u8>,
    pick: Option<f32>,
    tone: Option<f32>,
    damp: Option<f32>,
    lp: Option<f32>,
}

/// Peels mutation tokens off the right of `rest` until one is not a mutation; what is
/// left is the source, which may itself contain spaces.
fn split_mutations(rest: &str) -> (&str, Muts) {
    let mut m = Muts::default();
    let mut end = rest.len();
    loop {
        let head = rest[..end].trim_end();
        let tok_start = head.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
        let tok = &head[tok_start..];
        let took = match tok.split_once('=') {
            Some((k, v)) => {
                let f = v.parse::<f32>().ok();
                match k {
                    "pitch" => { m.pitch = f; true }
                    "crush" => { m.crush = v.parse().ok(); true }
                    "drive" => { m.drive = f; true }
                    "gain"  => { m.gain = f; true }
                    "f0"    => { m.f0 = f; true }
                    "sweep" => { m.sweep = f; true }
                    "decay" => { m.decay = f; true }
                    "click" => { m.click = f; true }
                    "wave"  => { m.wave = match v { "saw" => Some(0), "square" => Some(1), _ => None }; true }
                    "detune" => { m.detune = f; true }
                    "sub"   => { m.sub = f; true }
                    "cutoff" => { m.cutoff = f; true }
                    "floor" => { m.floor = f; true }
                    "res"   => { m.res = f; true }
                    "wob"   => { m.wob = f; true }
                    "hold"  => { m.hold = f; true }
                    "note"  => { m.note = v.parse().ok(); true }
                    "pick"  => { m.pick = f; true }
                    "tone"  => { m.tone = f; true }
                    "damp"  => { m.damp = f; true }
                    "lp"    => { m.lp = f; true }
                    _ => false,
                }
            }
            None => if tok == "rev" { m.rev = true; true } else { false },
        };
        if !took || tok_start == 0 { break; }
        end = tok_start;
    }
    (rest[..end].trim_end(), m)
}

/// Applies mutations in a fixed order: pitch, reverse, crush, drive, gain.
fn mutate(mono: &mut Vec<f32>, m: &Muts) {
    if let Some(r) = m.pitch.filter(|r| *r > 0.0 && (*r - 1.0).abs() > 1e-6) {
        // Resample by reading the source at a fractional cursor advancing `r` per output
        // sample -- the same linear interpolation live.rs uses, done once here.
        let n = (mono.len() as f32 / r) as usize;
        let last = mono.len().saturating_sub(1);
        let out: Vec<f32> = (0..n).map(|i| {
            let p = i as f32 * r;
            let k = p as usize;
            if k >= last { return 0.0; }
            let f = p - k as f32;
            mono[k] * (1.0 - f) + mono[k + 1] * f
        }).collect();
        *mono = out;
    }
    if m.rev { mono.reverse(); }
    if let Some(bits) = m.crush.filter(|b| (1..16).contains(b)) {
        let levels = (1u32 << bits) as f32;
        for s in mono.iter_mut() { *s = (*s * levels).round() / levels; }
    }
    if let Some(d) = m.drive.filter(|d| *d > 0.0) {
        // tanh saturation, normalised so a full-scale input still peaks at 1.0. Higher
        // drive squashes more of the waveform into the knee: that is the big-beat wall.
        let norm = d.tanh();
        for s in mono.iter_mut() { *s = (*s * d).tanh() / norm; }
    }
    if let Some(g) = m.gain { for s in mono.iter_mut() { *s *= g; } }
}

#[derive(Clone, Debug)]
pub struct KickParams { pub f0: f32, pub sweep: f32, pub decay_ms: f32, pub click: f32 }

impl Default for KickParams {
    fn default() -> KickParams { KickParams { f0: 50.0, sweep: 120.0, decay_ms: 400.0, click: 0.3 } }
}

/// What a voice is made of. Every kind of voice goes through this one enum: adding a fifth
/// means a variant, a `build` arm and a `render_baked` arm, and nothing else in the tree.
///
/// `build` owns the whole key table — the mapping from a kit line's mutation tokens to a
/// voice's parameters lives here and only here. `render_baked` produces `Voice.mono` at the
/// project rate: for a sample that IS the voice, and for a synth it is a preview for the
/// mixers that read `mono` directly (bench `show`/`live`, bake's fallback WAV) while the
/// Transport synthesises the real thing per hit.
#[derive(Clone, Debug)]
pub enum Source {
    Sample { path: PathBuf },
    Kick(KickParams),
    Wub(WubParams),
    String(string::Params),
}

impl Source {
    /// `<source>` plus its peeled mutations → a Source. Kit-level pitch/drive/crush/gain fold
    /// into a synth's own parameters here; for a sample they stay in `Muts` and are baked
    /// into the buffer afterwards by `mutate` (see `takes_mutations`).
    fn build(source: &str, m: &Muts) -> Result<Source, String> {
        match source {
            "synth:kick" => Ok(Source::Kick(KickParams {
                f0: m_or(m.f0, 50.0), sweep: m_or(m.sweep, 120.0),
                decay_ms: m_or(m.decay, 400.0), click: m_or(m.click, 0.3),
            })),
            "synth:wub" => {
                let d = WubParams::default();
                Ok(Source::Wub(WubParams {
                    f0: m_or(m.f0, d.f0) * m_or(m.pitch, 1.0),
                    wave: m.wave.unwrap_or(d.wave),
                    detune_cents: m_or(m.detune, d.detune_cents),
                    sub: m_or(m.sub, d.sub),
                    cutoff: m_or(m.cutoff, d.cutoff),
                    floor: m_or(m.floor, d.floor),
                    res: m_or(m.res, d.res),
                    wob: m_or(m.wob, d.wob),
                    hold_ms: m_or(m.hold, d.hold_ms),
                    decay_ms: m_or(m.decay, d.decay_ms),
                    drive: if m.drive.is_some() { m.drive } else { d.drive },
                    crush: m.crush,
                    gain: m_or(m.gain, 1.0),
                }))
            }
            "synth:string" => {
                let d = string::Params::default();
                Ok(Source::String(string::Params {
                    note: m.note.unwrap_or(d.note),
                    pick: m.pick.unwrap_or(d.pick),
                    tone: m.tone.unwrap_or(d.tone),
                    damp: m.damp.unwrap_or(d.damp),
                    decay_ms: m.decay.unwrap_or(d.decay_ms),
                    lp_hz: m.lp.unwrap_or(d.lp_hz),
                }))
            }
            s => match s.strip_prefix("synth:") {
                Some(other) => Err(format!("unknown synth '{other}' (have: kick, wub, string)")),
                None => Ok(Source::Sample { path: PathBuf::from(s) }),
            },
        }
    }

    /// The baked buffer at `rate`. A sample is read from disk and resampled to the project
    /// rate if it was recorded at another one; a synth generates at `rate` directly, so there
    /// is nothing to resample.
    fn render_baked(&self, rate: u32, dir: &Path) -> Result<Vec<f32>, String> {
        Ok(match self {
            Source::Sample { path } => {
                let p = dir.join(path);
                let bytes = std::fs::read(&p).map_err(|e| format!("{}: {e}", path.display()))?;
                let w = Wav::parse(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
                resample(&w.mono(), w.rate, rate)
            }
            Source::Kick(p) => synth_kick(rate, p),
            Source::Wub(p) => wub::preview(p, rate),
            Source::String(p) => string_preview(p, rate),
        })
    }

    /// True when kit-level pitch/rev/crush/drive/gain are baked into the buffer rather than
    /// folded into the voice's own parameters by `build`.
    fn takes_mutations(&self) -> bool {
        matches!(self, Source::Sample { .. } | Source::Kick(_))
    }
}

/// Windowed-sinc resample of `src` from `from` Hz to `to` Hz. Identical rates return the
/// input untouched — that exactness is what keeps a 44.1 kHz project bit-for-bit identical
/// to what the kit rendered before the project rate existed.
///
/// The kernel is a Blackman-windowed sinc, 32 zero crossings wide, with the cutoff pulled
/// down to the lower of the two Nyquists so downsampling filters rather than aliases.
pub fn resample(src: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || src.is_empty() { return src.to_vec(); }
    let ratio = to as f64 / from as f64;
    let cutoff = if ratio < 1.0 { ratio } else { 1.0 };
    const ZC: i64 = 32;
    // Kernel half-width in SOURCE samples: a lowered cutoff stretches the sinc, so the
    // window has to grow with it or the filter is truncated where it still has energy.
    let half = (ZC as f64 / cutoff).ceil() as i64;
    let n_out = ((src.len() as f64) * ratio).round() as usize;
    let last = src.len() as i64 - 1;
    (0..n_out).map(|i| {
        let center = i as f64 / ratio;
        let base = center.floor() as i64;
        let mut acc = 0.0f64;
        let mut norm = 0.0f64;
        for k in (base - half + 1)..=(base + half) {
            let x = center - k as f64;
            let w = {
                // Blackman over the kernel's own width, zero at the ends.
                let t = (x / half as f64 + 1.0) * 0.5;
                if !(0.0..=1.0).contains(&t) { 0.0 }
                else {
                    let tau = std::f64::consts::TAU;
                    0.42 - 0.5 * (tau * t).cos() + 0.08 * (2.0 * tau * t).cos()
                }
            };
            if w == 0.0 { continue; }
            let s = {
                let px = std::f64::consts::PI * x * cutoff;
                if px.abs() < 1e-12 { cutoff } else { cutoff * px.sin() / px }
            };
            let tap = s * w;
            // Clamp-extend the edges rather than zero-padding: zeros would fade the first
            // and last few samples of a one-shot, which is exactly where a drum transient is.
            acc += tap * src[k.clamp(0, last) as usize] as f64;
            norm += tap;
        }
        (if norm.abs() > 1e-12 { acc / norm } else { 0.0 }) as f32
    }).collect()
}

/// The 808 in one function: a sine whose frequency falls exponentially from `sweep` to
/// `f0` over the first ~60 ms and whose amplitude decays exponentially to -60 dB at
/// `decay` ms, with a few milliseconds of noise on the front for the beater. Drive it
/// with the `drive=` mutation for the Prodigy wall; this is deliberately clean on its own.
fn synth_kick(rate: u32, p: &KickParams) -> Vec<f32> {
    let KickParams { f0, sweep, decay_ms, click } = *p;
    let sr = rate as f32;
    let n = (sr * decay_ms / 1000.0 * 1.2) as usize;
    let amp_k = -(1000.0f32.ln()) / (sr * decay_ms / 1000.0);   // -60 dB at decay_ms
    let pitch_k = -1.0 / (sr * 0.020);                            // sweep time constant 20 ms
    let mut phase = 0.0f32;
    let mut seed = 0x2545F491u32;
    (0..n).map(|i| {
        let t = i as f32;
        let f = f0 + (sweep - f0) * (pitch_k * t).exp();
        phase += 2.0 * std::f32::consts::PI * f / sr;
        let body = phase.sin() * (amp_k * t).exp();
        // xorshift noise, 4 ms, fast decay: the beater hitting the head
        seed ^= seed << 13; seed ^= seed >> 17; seed ^= seed << 5;
        let noise = ((seed >> 8) as f32 / 16_777_216.0 * 2.0 - 1.0)
            * click * (-(t) / (sr * 0.004)).exp();
        body + noise
    }).collect()
}

#[derive(Clone, Debug)]
pub struct Voice {
    pub label: String,
    pub color: [u8; 3],
    /// The baked buffer at the kit's rate. For a sample this IS the voice; for a synth it is
    /// a fixed preview for the mixers that read `mono` directly, since the Transport
    /// synthesises those per hit from `source` instead.
    pub mono: Vec<f32>,
    pub source: Source,
}

#[derive(Debug)]
pub struct Kit {
    pub name: String,
    /// The rate everything in this kit was rendered at — the project rate it was loaded for,
    /// never "whatever the first sample happened to be".
    pub rate: u32,
    pub voices: [Option<Voice>; 9],
}

impl Kit {
    /// Loads a kit FOR a project rate. Every voice — sample or synth — ends up rendered at
    /// `rate`, so the mixer never has to ask what rate a given voice is in.
    pub fn load(path: &Path, rate: u32) -> Result<Kit, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let dir = path.parent().unwrap_or(Path::new("."));
        let mut kit = Kit { name: "untitled".into(), rate, voices: Default::default() };
        for (ln, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            let mut it = line.splitn(2, ' ');
            let key = it.next().unwrap();
            let rest = it.next().unwrap_or("").trim();
            match key {
                "name" => kit.name = rest.to_string(),
                "voice" => {
                    // slot label colour path — path may contain spaces, so peel 3 tokens
                    let mut s = rest;
                    let mut tok = || -> Option<&str> {
                        s = s.trim_start();
                        let end = s.find(char::is_whitespace).unwrap_or(s.len());
                        let (t, r) = s.split_at(end);
                        s = r;
                        (!t.is_empty()).then_some(t)
                    };
                    let slot: usize = tok().and_then(|s| s.parse().ok()).filter(|&s| s < 9)
                        .ok_or(format!("line {}: slot must be 0-8", ln + 1))?;
                    let label = tok().ok_or("missing label")?.to_string();
                    let color = parse_rgb(tok().ok_or("missing colour")?)?;
                    // The remainder is `<source> [mutations...]`. The source may contain
                    // spaces (sample folders do), so mutations are peeled off the RIGHT:
                    // trailing tokens that look like key=value or a known flag.
                    let (source, muts) = split_mutations(s.trim());
                    if source.is_empty() { return Err(format!("line {}: missing source", ln + 1)); }

                    let source = Source::build(source, &muts).map_err(|e| format!("line {}: {e}", ln + 1))?;
                    let mut mono = source.render_baked(kit.rate, dir)
                        .map_err(|e| format!("line {}: {e}", ln + 1))?;
                    if source.takes_mutations() { mutate(&mut mono, &muts); }
                    kit.voices[slot] = Some(Voice { label, color, mono, source });
                }
                _ => return Err(format!("line {}: unknown key {key}", ln + 1)),
            }
        }
        if kit.voices.iter().all(|v| v.is_none()) { return Err("kit has no voices".into()); }
        Ok(kit)
    }
}

fn m_or(v: Option<f32>, d: f32) -> f32 { v.unwrap_or(d) }

/// A fixed-note render of a string voice at its own default note and full velocity, for the
/// sample-only mixers that read `Voice.mono` directly rather than ringing a live `Ks`.
fn string_preview(p: &string::Params, rate: u32) -> Vec<f32> {
    let mut ks = string::Ks::new(rate);
    ks.pluck(string::note_hz(p.note as f32), 1.0, p, p.damp);
    let n = (rate as f32 * p.decay_ms / 1000.0 * 1.2) as usize;
    (0..n).map(|_| ks.next()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole bit-exactness guarantee rests on this: a project at the rate its samples
    /// are already in must not be filtered, dithered or interpolated on the way in.
    #[test]
    fn resampling_to_the_same_rate_is_the_identity() {
        let src: Vec<f32> = (0..1000).map(|i| (i as f32 * 0.037).sin()).collect();
        let out = resample(&src, 44100, 44100);
        assert_eq!(out, src, "equal rates must return the input untouched, bit for bit");
    }

    #[test]
    fn resampling_changes_length_by_the_rate_ratio() {
        let src = vec![0.0f32; 4410];
        assert_eq!(resample(&src, 44100, 48000).len(), 4800);
        assert_eq!(resample(&src, 44100, 22050).len(), 2205);
        assert!(resample(&[], 44100, 48000).is_empty());
    }

    /// A sine well below both Nyquists must survive a rate change with its shape intact --
    /// the test that catches a kernel that is windowed wrongly or normalised wrongly.
    #[test]
    fn a_sine_survives_a_rate_change() {
        const HZ: f32 = 440.0;
        let src: Vec<f32> = (0..44100)
            .map(|i| (std::f32::consts::TAU * HZ * i as f32 / 44100.0).sin()).collect();
        let out = resample(&src, 44100, 48000);
        // Compare against the sine the target rate should have produced, away from the
        // edges where the kernel's clamp-extension legitimately differs.
        let err = (2000..46000).map(|i| {
            let want = (std::f32::consts::TAU * HZ * i as f32 / 48000.0).sin();
            (out[i] - want).abs()
        }).fold(0f32, f32::max);
        assert!(err < 0.01, "resampled sine drifted from the ideal by {err}");
    }

    /// Downsampling has to filter before it decimates. A tone above the target's Nyquist
    /// must come back quiet rather than folding down as a phantom low note.
    #[test]
    fn downsampling_filters_instead_of_aliasing() {
        // 15 kHz into a 22.05 kHz project: Nyquist is 11.025 kHz, so this must be rejected.
        let src: Vec<f32> = (0..44100)
            .map(|i| (std::f32::consts::TAU * 15000.0 * i as f32 / 44100.0).sin()).collect();
        let out = resample(&src, 44100, 22050);
        let peak = out[500..out.len() - 500].iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(peak < 0.1, "a tone above the target Nyquist aliased through at {peak}");
    }

    #[test]
    fn the_source_builder_owns_the_key_table() {
        let (src, m) = split_mutations("synth:kick f0=44 decay=420");
        assert_eq!(src, "synth:kick");
        match Source::build(src, &m).unwrap() {
            Source::Kick(k) => { assert_eq!(k.f0, 44.0); assert_eq!(k.decay_ms, 420.0); }
            other => panic!("expected a kick, got {other:?}"),
        }
        let (src, m) = split_mutations("drums/snare.wav pitch=0.85 drive=4");
        assert!(matches!(Source::build(src, &m).unwrap(), Source::Sample { .. }));
        assert_eq!(m.pitch, Some(0.85));
        assert!(Source::build("synth:nope", &Muts::default()).is_err());
    }

    /// A sample bakes its kit-level mutations into the buffer; a synth folds them into its
    /// own parameters instead, so applying them again afterwards would double them.
    #[test]
    fn only_buffer_sources_take_baked_mutations() {
        assert!(Source::Sample { path: "x.wav".into() }.takes_mutations());
        assert!(Source::Kick(KickParams::default()).takes_mutations());
        assert!(!Source::Wub(WubParams::default()).takes_mutations());
        assert!(!Source::String(string::Params::default()).takes_mutations());
    }
}

fn parse_rgb(s: &str) -> Result<[u8; 3], String> {
    let v: Vec<u8> = s.split(',').map(|x| x.trim().parse().map_err(|_| format!("bad colour {s}"))).collect::<Result<_, _>>()?;
    if v.len() != 3 { return Err(format!("bad colour {s}")); }
    Ok([v[0], v[1], v[2]])
}
