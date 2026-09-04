//! A kit = up to 9 voices (one per annunciator tile, 3×3) each backed by one toykeyboards hit.
//! Kit file format (plain lines, `#` comments):
//!   name <kit name>
//!   voice <slot 0-8> <label> <r,g,b> <source> [mutations...]
//! Paths are relative to the kit file's directory.
//!
//! <source> is a WAV path, or `synth:kick`. Mutations are trailing tokens, applied in the
//! order listed below and BAKED into the voice at load — zero cost at play time:
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
//! This is the whole "make it sound like Prodigy instead of a music class" mechanism:
//! those records are samples mutated hard and looped at the right cadence. Two references
//! measured (tools/analyze_track.py): half of everything sits below 120 Hz and hats are
//! under 1%, so the sub kick is a sine with a pitch drop -- a sample cannot give you that,
//! but an 808 is exactly a sine -- and the toy-keyboard hits get pitched down and driven.

use crate::wav::Wav;
use std::path::Path;

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

/// Generates a synth voice. Only `kick` exists so far.
fn synth(kind: &str, rate: u32, m: &Muts) -> Result<Vec<f32>, String> {
    match kind {
        "kick" => Ok(synth_kick(rate,
            m.f0.unwrap_or(50.0), m.sweep.unwrap_or(120.0),
            m.decay.unwrap_or(400.0), m.click.unwrap_or(0.3))),
        other => Err(format!("unknown synth '{other}' (have: kick)")),
    }
}

/// The 808 in one function: a sine whose frequency falls exponentially from `sweep` to
/// `f0` over the first ~60 ms and whose amplitude decays exponentially to -60 dB at
/// `decay` ms, with a few milliseconds of noise on the front for the beater. Drive it
/// with the `drive=` mutation for the Prodigy wall; this is deliberately clean on its own.
fn synth_kick(rate: u32, f0: f32, sweep: f32, decay_ms: f32, click: f32) -> Vec<f32> {
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
    pub mono: Vec<f32>,
}

#[derive(Debug)]
pub struct Kit {
    pub name: String,
    pub rate: u32,
    pub voices: [Option<Voice>; 9],
}

impl Kit {
    pub fn load(path: &Path) -> Result<Kit, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let dir = path.parent().unwrap_or(Path::new("."));
        let mut kit = Kit { name: "untitled".into(), rate: 0, voices: Default::default() };
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

                    let mut mono = if let Some(kind) = source.strip_prefix("synth:") {
                        // A synth voice may precede any sample; it needs a rate before one
                        // has been seen, so default to CD rate and let a later sample
                        // disagree loudly rather than silently resample.
                        if kit.rate == 0 { kit.rate = 44100; }
                        synth(kind, kit.rate, &muts).map_err(|e| format!("line {}: {e}", ln + 1))?
                    } else {
                        let bytes = std::fs::read(dir.join(source)).map_err(|e| format!("{source}: {e}"))?;
                        let w = Wav::parse(&bytes).map_err(|e| format!("{source}: {e}"))?;
                        if kit.rate == 0 { kit.rate = w.rate; }
                        else if w.rate != kit.rate { return Err(format!("{source}: rate {} != kit rate {}", w.rate, kit.rate)); }
                        w.mono()
                    };
                    mutate(&mut mono, &muts);
                    kit.voices[slot] = Some(Voice { label, color, mono });
                }
                _ => return Err(format!("line {}: unknown key {key}", ln + 1)),
            }
        }
        if kit.rate == 0 { return Err("kit has no voices".into()); }
        Ok(kit)
    }
}

fn parse_rgb(s: &str) -> Result<[u8; 3], String> {
    let v: Vec<u8> = s.split(',').map(|x| x.trim().parse().map_err(|_| format!("bad colour {s}"))).collect::<Result<_, _>>()?;
    if v.len() != 3 { return Err(format!("bad colour {s}")); }
    Ok([v[0], v[1], v[2]])
}
