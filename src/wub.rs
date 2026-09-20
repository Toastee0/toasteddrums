//! The wub: a bass synth voice that is computed per sample instead of read from a buffer,
//! so the wobble stays locked to the tempo whatever note a hit asks for. Kit line:
//!
//!   voice 5 wub 0,255,255 synth:wub f0=55 wave=saw detune=7 sub=0.6 cutoff=1400 floor=90 res=0.75 wob=2 hold=250 decay=700 drive=3
//!
//!   f0      base note in Hz (55 = A1). Per-hit `pitch` multiplies it; the wobble does not move.
//!   wave    saw | square. Two of them, detuned +-`detune` cents against each other.
//!   sub     level of a sine at f0 under the oscillators: the 808 fundamental.
//!   cutoff  the filter's open frequency; `floor` its closed one. The LFO sweeps between
//!           them exponentially, closed at the start of the note.
//!   res     0..1 resonance. 0.75 honks; 0.9 screams.
//!   wob     wobbles per beat: 2 = eighth notes, 4 = sixteenths, 0.5 = a slow breath.
//!   hold    ms at full level before the release; `decay` ms from there to -60 dB.
//!           A per-hit decay_ms overrides `decay`.
//!   drive / crush / gain   post-filter, as for samples. `rev` is meaningless and ignored.
//!
//! Every operation here is mirrored in the game's C port (Sand walker, src/tracker.c). Keep
//! the order of operations identical when changing anything: the golden test compares the two
//! renders against a TOLERANCE (max-abs 2e-3, RMS 1e-4 — see that repo's
//! tests/test_tracker.c), not sample for sample. The two agree exactly on Windows only
//! because Rust windows-gnu and GCC link the same mingw-w64 libm; `next` below calls sin,
//! cos, powf and tanh per sample into a recursive filter, so on another libm the last ulp
//! compounds. Exact equality is a property of one toolchain, never of this code.

use std::f32::consts::PI;

#[derive(Clone, Debug, PartialEq)]
pub struct WubParams {
    pub f0: f32,
    /// 0 saw, 1 square
    pub wave: u8,
    pub detune_cents: f32,
    pub sub: f32,
    pub cutoff: f32,
    pub floor: f32,
    pub res: f32,
    pub wob: f32,
    pub hold_ms: f32,
    pub decay_ms: f32,
    pub drive: Option<f32>,
    pub crush: Option<u32>,
    pub gain: f32,
}

impl Default for WubParams {
    fn default() -> Self {
        WubParams { f0: 55.0, wave: 0, detune_cents: 7.0, sub: 0.6, cutoff: 1400.0, floor: 90.0,
                    res: 0.75, wob: 2.0, hold_ms: 250.0, decay_ms: 700.0, drive: Some(3.0), crush: None, gain: 1.0 }
    }
}

/// A sounding wub note.
#[derive(Clone, Debug)]
pub struct WubState {
    sr: f32,
    wave: u8,
    ph_a: f32, ph_b: f32, ph_sub: f32,   // oscillator phases 0..1
    inc_a: f32, inc_b: f32, inc_sub: f32,
    sub: f32,
    lfo_ph: f32, lfo_inc: f32,
    lfo_floor: f32, lfo_ratio: f32,      // cutoff = floor * ratio^depth
    q: f32,
    low: f32, band: f32,                 // Chamberlin SVF state
    env: f32,
    hold_left: u32,
    attack_left: u32,
    attack_step: f32,
    release_k: f32,
    drive: Option<f32>, drive_norm: f32,
    crush: Option<f32>,
    gain: f32,
    done: bool,
}

impl WubState {
    /// `pitch` scales f0; `bpm` sets the wobble rate; `decay_override` is a per-hit decay_ms.
    pub fn new(p: &WubParams, sr: u32, pitch: f32, bpm: f32, decay_override: Option<f32>) -> WubState {
        let sr = sr as f32;
        let f = p.f0 * pitch;
        let det = (2.0f32).powf(p.detune_cents / 1200.0);
        let decay_ms = decay_override.filter(|d| *d > 0.0).unwrap_or(p.decay_ms).max(1.0);
        let attack = (sr * 0.002) as u32;                      // 2 ms linear
        WubState {
            sr, wave: p.wave,
            ph_a: 0.0, ph_b: 0.5, ph_sub: 0.0,
            inc_a: f * det / sr, inc_b: f / det / sr, inc_sub: f / sr,
            sub: p.sub,
            lfo_ph: 0.0, lfo_inc: p.wob * bpm / 60.0 / sr,
            lfo_floor: p.floor.max(20.0),
            lfo_ratio: (p.cutoff.max(p.floor.max(20.0)) / p.floor.max(20.0)).max(1.0),
            q: 2.0 - 1.9 * p.res.clamp(0.0, 1.0),
            low: 0.0, band: 0.0,
            env: 0.0,
            hold_left: (sr * p.hold_ms / 1000.0) as u32,
            attack_left: attack.max(1),
            attack_step: 1.0 / attack.max(1) as f32,
            release_k: (-(1000.0f32.ln()) / (sr * decay_ms / 1000.0)).exp(),
            drive: p.drive.filter(|d| *d > 0.0),
            drive_norm: p.drive.filter(|d| *d > 0.0).map(|d| d.tanh()).unwrap_or(1.0),
            crush: p.crush.filter(|b| (1..16).contains(b)).map(|b| (1u32 << b) as f32),
            gain: p.gain,
            done: false,
        }
    }

    pub fn done(&self) -> bool { self.done }

    /// One sample. Returns 0 forever once the note has released.
    pub fn next(&mut self) -> f32 {
        if self.done { return 0.0; }
        // oscillators
        let osc = |ph: f32, wave: u8| -> f32 {
            if wave == 1 { if ph < 0.5 { 1.0 } else { -1.0 } } else { 2.0 * ph - 1.0 }
        };
        let a = osc(self.ph_a, self.wave);
        let b = osc(self.ph_b, self.wave);
        let s = (2.0 * PI * self.ph_sub).sin();
        let x = (a + b) * 0.5 * (1.0 - self.sub * 0.5) + s * self.sub;
        self.ph_a += self.inc_a; if self.ph_a >= 1.0 { self.ph_a -= 1.0; }
        self.ph_b += self.inc_b; if self.ph_b >= 1.0 { self.ph_b -= 1.0; }
        self.ph_sub += self.inc_sub; if self.ph_sub >= 1.0 { self.ph_sub -= 1.0; }

        // LFO: closed at note start, opens to `cutoff`, closes again. Exponential sweep.
        let depth = 0.5 - 0.5 * (2.0 * PI * self.lfo_ph).cos();
        self.lfo_ph += self.lfo_inc; if self.lfo_ph >= 1.0 { self.lfo_ph -= 1.0; }
        let fc = self.lfo_floor * self.lfo_ratio.powf(depth);
        // Chamberlin SVF, low-pass out. f = 2 sin(pi fc / sr), clamped for stability.
        let f = (2.0 * (PI * fc / self.sr).sin()).min(0.9);
        self.low += f * self.band;
        let high = x - self.low - self.q * self.band;
        self.band += f * high;
        let mut y = self.low;

        // envelope: attack, hold, release
        if self.attack_left > 0 {
            self.attack_left -= 1;
            self.env += self.attack_step;
            if self.env > 1.0 { self.env = 1.0; }
        } else if self.hold_left > 0 {
            self.hold_left -= 1;
        } else {
            self.env *= self.release_k;
            if self.env < 0.001 { self.done = true; return 0.0; }
        }
        y *= self.env * self.gain;

        if let Some(lv) = self.crush { y = (y * lv).round() / lv; }
        if let Some(d) = self.drive { y = (y * d).tanh() / self.drive_norm; }
        y
    }
}

/// A preview buffer for the mixers that only read samples (the .pat bench and live pads):
/// the base note at 120 bpm, rendered to silence.
pub fn preview(p: &WubParams, sr: u32) -> Vec<f32> {
    let mut st = WubState::new(p, sr, 1.0, 120.0, None);
    let cap = sr as usize * 4;
    let mut out = Vec::with_capacity(cap / 2);
    while !st.done() && out.len() < cap { out.push(st.next()); }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wub_has_a_fundamental_and_ends() {
        let p = WubParams::default();
        let v = preview(&p, 44100);
        // hold 250 ms + release 700 ms: done well inside 1.5 s
        assert!(v.len() > 44100 / 2 && v.len() < 44100 * 3 / 2, "len {}", v.len());
        let peak = v.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.3 && peak <= 1.0 / (3.0f32).tanh() + 1e-4, "peak {peak}");   // drive: tanh(3y)/tanh(3) tops out just over 1
        // the wobble: the filter is closed at the start and open a quarter-wobble later, so the
        // high-frequency content (first difference) must be larger there. Total energy is no
        // use: the 55 Hz fundamental passes a 90 Hz low-pass anyway.
        let hf = |a: usize, b: usize| (a + 1..b).map(|i| (v[i] - v[i - 1]).powi(2)).sum::<f32>() / (b - a) as f32;
        let wobble = 44100 / 4;   // at 120 bpm, wob=2: 4 Hz, 250 ms per wobble
        assert!(hf(wobble / 2 - wobble / 16, wobble / 2 + wobble / 16) > 2.0 * hf(wobble / 32, wobble / 8), "no wobble");
    }

    #[test]
    fn pitch_changes_note_not_wobble() {
        let p = WubParams::default();
        let a = WubState::new(&p, 44100, 1.0, 120.0, None);
        let b = WubState::new(&p, 44100, 2.0, 120.0, None);
        assert_eq!(a.lfo_inc, b.lfo_inc);
        assert!((b.inc_sub - a.inc_sub * 2.0).abs() < 1e-9);
    }
}
