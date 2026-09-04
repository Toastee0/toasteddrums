//! song — the tracker's data model and its real-time transport.
//!
//! MODEL. A Song is one BPM and any number of Tracks. A Track has its own length in
//! sixteenth-note steps (16 = a bar of 4/4, 12 = 3/4, 14 = 7/8, 5 = whatever you like) and
//! they all loop independently against the shared clock, so polymeter is the default rather
//! than a feature. Each cell holds zero or more Hits, and each Hit carries its own Mods --
//! pitch, drive, crush, reverse, gain, decay -- which override the voice's baked defaults
//! for THAT hit only. That is the "any modifier, any note" rule: nothing is per-track that
//! could be per-hit.
//!
//! A Track also carries a pad map: which kit slot each of the four plates plays while this
//! track is the current context. One instrument, many tracks.
//!
//! TRANSPORT. Runs inside the audio callback and owns the kit. Produces kit-rate audio
//! honouring step boundaries; a small resampler feeds the device at its native rate. Mods
//! are applied per sample at mix time, which is cheap at a dozen voices.
//!
//! Everything here is serde-serialisable: the song file is JSON, and the MCP tools take and
//! return these exact shapes.

use crate::kit::Kit;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Per-hit overrides. All optional; None means "the voice's own default".
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Mods {
    /// playback speed multiplier: 0.5 = octave down, 2.0 = up
    #[serde(default, skip_serializing_if = "Option::is_none")] pub pitch: Option<f32>,
    /// tanh saturation drive: 1 gentle, 4 wall, 10 fuzz
    #[serde(default, skip_serializing_if = "Option::is_none")] pub drive: Option<f32>,
    /// bit depth 1-15: 8 gritty, 4 destroyed
    #[serde(default, skip_serializing_if = "Option::is_none")] pub crush: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub rev: Option<bool>,
    /// linear level multiplier
    #[serde(default, skip_serializing_if = "Option::is_none")] pub gain: Option<f32>,
    /// force the hit to fade to -60 dB by this many ms, regardless of sample length
    #[serde(default, skip_serializing_if = "Option::is_none")] pub decay_ms: Option<f32>,
}

impl Mods {
    /// `other` wins where it is Some. Used to stack armed mods over a recorded hit's own.
    pub fn over(&self, other: &Mods) -> Mods {
        Mods {
            pitch: other.pitch.or(self.pitch),
            drive: other.drive.or(self.drive),
            crush: other.crush.or(self.crush),
            rev: other.rev.or(self.rev),
            gain: other.gain.or(self.gain),
            decay_ms: other.decay_ms.or(self.decay_ms),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Hit {
    /// kit slot 0-8
    pub slot: usize,
    /// 1-127
    pub vel: u8,
    #[serde(default)]
    pub mods: Mods,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Track {
    pub name: String,
    /// length in sixteenth steps; any positive integer
    pub len: usize,
    /// which kit slot each pad (0-3) plays while this track is the context
    #[serde(default = "default_pads")]
    pub pads: [usize; 4],
    /// MIDI note -> kit slot for drum mode while this track is the keys context. Empty means
    /// the GM-ish default in `slot_for_note`. serde_json writes the u8 keys as strings and
    /// reads them back, so song_get/song_set stay symmetric.
    #[serde(default)]
    pub keys: BTreeMap<u8, usize>,
    /// cells[step] = the hits on that step
    #[serde(default)]
    pub cells: Vec<Vec<Hit>>,
    #[serde(default)]
    pub mute: bool,
}

fn default_pads() -> [usize; 4] { [0, 3, 1, 8] }

impl Track {
    pub fn new(name: &str, len: usize) -> Track {
        let len = len.max(1);
        Track { name: name.into(), len, pads: default_pads(), keys: BTreeMap::new(),
                cells: vec![Vec::new(); len], mute: false }
    }
    /// Keep `cells` exactly `len` long after any edit, preserving what fits.
    pub fn normalise(&mut self) {
        self.len = self.len.max(1);
        self.cells.resize(self.len, Vec::new());
    }
    /// Drum-mode mapping for a MIDI note: the track's own map, else General MIDI's
    /// percussion layout for the nine slots (kick, snare, stick, hat, open hat, cowbell,
    /// low tom, high tom, crash). None means the key plays nothing.
    pub fn slot_for_note(&self, note: u8) -> Option<usize> {
        if let Some(&s) = self.keys.get(&note) { return Some(s); }
        if !self.keys.is_empty() { return None; }
        match note {
            36 => Some(0), 38 => Some(1), 37 => Some(2), 42 => Some(3), 46 => Some(4),
            56 => Some(5), 45 => Some(6), 50 => Some(7), 49 => Some(8),
            _ => None,
        }
    }
}

/// Nearest step to "now" for quantising a live hit into a track of `len` steps.
/// `phase` is the fraction of the current global step already elapsed. It updates once per
/// audio callback (~10 ms), i.e. ~8% of a 125 ms step at 120 BPM -- fine for nearest-step.
pub fn quantise(global_step: u64, phase: f32, len: usize) -> usize {
    let target = if phase < 0.5 { global_step } else { global_step + 1 };
    (target % len.max(1) as u64) as usize
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Song {
    pub bpm: f32,
    #[serde(default)]
    pub tracks: Vec<Track>,
}

impl Song {
    pub fn empty(bpm: f32) -> Song {
        Song { bpm, tracks: vec![Track::new("drums", 16)] }
    }
    pub fn normalise(&mut self) {
        if !(20.0..=300.0).contains(&self.bpm) { self.bpm = 120.0; }
        for t in &mut self.tracks { t.normalise(); }
    }
    pub fn samples_per_step(&self, rate: u32) -> usize {
        ((rate as f32 * 60.0 / self.bpm / 4.0) as usize).max(1)
    }
    /// Least common multiple of track lengths: one full cycle of the polymeter.
    pub fn cycle_steps(&self) -> usize {
        fn gcd(a: usize, b: usize) -> usize { if b == 0 { a } else { gcd(b, a % b) } }
        self.tracks.iter().map(|t| t.len).fold(1, |acc, l| acc / gcd(acc, l) * l)
    }
}

// ---------------------------------------------------------------------------------------

/// A sounding hit with its mods resolved into per-instance state.
struct Voice {
    slot: usize,
    pos: f32,       // fractional source cursor
    step: f32,      // cursor advance per kit-rate sample (pitch)
    rev: bool,
    gain: f32,
    drive: Option<f32>,
    crush: Option<f32>, // levels
    decay_k: Option<f32>, // per-sample amplitude multiplier for decay_ms
    env: f32,
}

pub enum Cmd {
    SetSong(Song),
    SetKit(Arc<Kit>),
    Play,
    Stop,
    /// Play a hit right now, outside the grid (live pad play-through).
    Trigger(Hit),
    Master(f32),
}

/// Owns the kit and the song inside the audio callback. Produces KIT-RATE audio.
pub struct Transport {
    pub kit: Arc<Kit>,
    pub song: Song,
    pub playing: bool,
    pub global_step: u64,
    pos_in_step: usize,
    voices: Vec<Voice>,
    pub master: f32,
    /// per-slot glow 0..1 for any display, decays per mix
    pub glow: [f32; 9],
}

impl Transport {
    pub fn new(kit: Arc<Kit>, song: Song) -> Transport {
        Transport { kit, song, playing: false, global_step: 0, pos_in_step: 0,
                    voices: Vec::with_capacity(32), master: 2.0, glow: [0.0; 9] }
    }

    pub fn apply(&mut self, c: Cmd) {
        match c {
            Cmd::SetSong(mut s) => { s.normalise(); self.song = s; }
            Cmd::SetKit(k) => { self.voices.clear(); self.kit = k; }
            Cmd::Play => { self.playing = true; }
            Cmd::Stop => { self.playing = false; self.global_step = 0; self.pos_in_step = 0; }
            Cmd::Trigger(h) => self.start(&h),
            Cmd::Master(m) => self.master = m.max(0.0),
        }
    }

    /// Fraction of the current step elapsed, for quantising live hits.
    pub fn step_phase(&self) -> f32 {
        let spp = self.song.samples_per_step(self.kit.rate).max(1);
        self.pos_in_step as f32 / spp as f32
    }

    fn start(&mut self, h: &Hit) {
        let v = match self.kit.voices.get(h.slot).and_then(|v| v.as_ref()) { Some(v) => v, None => return };
        if self.voices.len() >= 32 { self.voices.remove(0); }
        let m = &h.mods;
        let len = v.mono.len() as f32;
        let rev = m.rev.unwrap_or(false);
        let sr = self.kit.rate as f32;
        self.voices.push(Voice {
            slot: h.slot,
            pos: if rev { (len - 2.0).max(0.0) } else { 0.0 },
            step: m.pitch.filter(|p| *p > 0.0).unwrap_or(1.0),
            rev,
            // (vel/127)^2: perceived loudness tracks power, linear velocity feels top-heavy
            gain: (h.vel.clamp(1, 127) as f32 / 127.0).powi(2) * m.gain.unwrap_or(1.0),
            drive: m.drive.filter(|d| *d > 0.0),
            crush: m.crush.filter(|b| (1..16).contains(b)).map(|b| (1u32 << b) as f32),
            decay_k: m.decay_ms.filter(|d| *d > 0.0).map(|d| (-(1000.0f32.ln()) / (sr * d / 1000.0)).exp()),
            env: 1.0,
        });
        self.glow[h.slot] = 1.0;
    }

    fn trigger_step(&mut self) {
        let gs = self.global_step;
        // collect first: triggering borrows self mutably
        let hits: Vec<Hit> = self.song.tracks.iter()
            .filter(|t| !t.mute && t.len > 0)
            .flat_map(|t| t.cells[(gs % t.len as u64) as usize].iter().cloned())
            .collect();
        for h in &hits { self.start(h); }
    }

    /// Fill `out` with kit-rate mono, advancing the clock. Additive over zeroed buffer.
    pub fn fill(&mut self, out: &mut [f32]) {
        for o in out.iter_mut() { *o = 0.0; }
        let spp = self.song.samples_per_step(self.kit.rate);
        let mut done = 0;
        while done < out.len() {
            // A BPM raise shrinks spp; if the cursor is already past the new step length,
            // roll into the next step rather than underflow the subtraction below.
            if self.playing && self.pos_in_step >= spp { self.pos_in_step = 0; self.global_step += 1; }
            if self.playing && self.pos_in_step == 0 { self.trigger_step(); }
            let n = if self.playing {
                spp.saturating_sub(self.pos_in_step).max(1).min(out.len() - done)
            } else { out.len() - done };
            self.mix(&mut out[done..done + n]);
            done += n;
            if self.playing {
                self.pos_in_step += n;
                if self.pos_in_step >= spp { self.pos_in_step = 0; self.global_step += 1; }
            }
        }
        let master = self.master;
        for o in out.iter_mut() {
            let v = *o * master;
            // same tanh knee as the offline mixer, so live and render sound alike
            *o = if v.abs() > 0.8 { v.signum() * (0.8 + (v.abs() - 0.8).tanh() * 0.2) } else { v };
        }
        let decay = (-(out.len() as f32) / (self.kit.rate as f32 * 0.12)).exp();
        for g in &mut self.glow { *g *= decay; }
    }

    fn mix(&mut self, out: &mut [f32]) {
        let kit = &self.kit;
        self.voices.retain_mut(|v| {
            let src = match kit.voices.get(v.slot).and_then(|s| s.as_ref()) { Some(s) => &s.mono, None => return false };
            let last = src.len().saturating_sub(1);
            if last == 0 { return false; }
            let norm = v.drive.map(|d| d.tanh());
            for o in out.iter_mut() {
                let i = v.pos as usize;
                if i >= last { return false; }
                let f = v.pos - i as f32;
                let mut s = (src[i] * (1.0 - f) + src[i + 1] * f) * v.gain * v.env;
                if let Some(lv) = v.crush { s = (s * lv).round() / lv; }
                if let (Some(d), Some(n)) = (v.drive, norm) { s = (s * d).tanh() / n; }
                *o += s;
                if v.rev { if v.pos < v.step { return false; } v.pos -= v.step; } else { v.pos += v.step; }
                if let Some(k) = v.decay_k { v.env *= k; if v.env < 0.001 { return false; } }
            }
            true
        });
    }

    /// Offline render of `cycles` full polymeter cycles (or `steps` if given) to kit-rate mono.
    pub fn render(kit: Arc<Kit>, song: Song, steps: usize) -> Vec<f32> {
        let mut t = Transport::new(kit, song);
        t.playing = true;
        let spp = t.song.samples_per_step(t.kit.rate);
        let mut out = vec![0.0f32; spp * steps];
        let mut pos = 0;
        while pos < out.len() {
            let n = (out.len() - pos).min(2048);
            t.fill(&mut out[pos..pos + n]);
            pos += n;
        }
        t.playing = false;
        let mut tail = vec![0.0f32; t.kit.rate as usize];  // one second for the last hit's ring
        t.fill(&mut tail);
        out.extend_from_slice(&tail);
        out
    }
}

/// Kit-rate → device-rate shim with a fractional cursor carried across calls (no clicks at
/// callback boundaries).
pub struct Resampler {
    ratio: f32,      // kit_rate / dev_rate
    buf: Vec<f32>,   // kit-rate samples not yet consumed
    pos: f32,
}

impl Resampler {
    pub fn new(kit_rate: u32, dev_rate: u32) -> Resampler {
        Resampler { ratio: kit_rate as f32 / dev_rate as f32, buf: Vec::with_capacity(4096), pos: 0.0 }
    }
    /// Produce `out.len()` device-rate samples, pulling kit-rate audio via `pull` as needed.
    pub fn run<F: FnMut(&mut [f32])>(&mut self, out: &mut [f32], mut pull: F) {
        let need = (out.len() as f32 * self.ratio) as usize + 2;
        while self.buf.len() < (self.pos as usize) + need {
            let start = self.buf.len();
            self.buf.resize(start + 1024, 0.0);
            pull(&mut self.buf[start..]);
        }
        // buf is always >= 1024 after the fill loop above, so `last >= 1` and the i+1 read
        // is bounded explicitly rather than by the +2 slack alone.
        let last = self.buf.len() - 1;
        for o in out.iter_mut() {
            let i = (self.pos as usize).min(last - 1);
            let f = self.pos - i as f32;
            *o = self.buf[i] * (1.0 - f) + self.buf[i + 1] * f;
            self.pos += self.ratio;
        }
        let consumed = (self.pos as usize).min(self.buf.len());
        self.buf.drain(..consumed);
        self.pos -= consumed as f32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_grows_and_shrinks_cells() {
        let mut t = Track::new("t", 4);
        t.cells[1].push(Hit { slot: 0, vel: 100, mods: Mods::default() });
        t.len = 6; t.normalise();
        assert_eq!(t.cells.len(), 6);
        assert_eq!(t.cells[1].len(), 1);
        t.len = 2; t.normalise();
        assert_eq!(t.cells.len(), 2);
        t.len = 0; t.normalise();
        assert_eq!((t.len, t.cells.len()), (1, 1));
    }

    #[test]
    fn cycle_is_lcm_of_track_lengths() {
        let mut s = Song::empty(120.0);
        s.tracks[0].len = 16;
        s.tracks.push(Track::new("b", 12));
        assert_eq!(s.cycle_steps(), 48);
        let mut s = Song { bpm: 120.0, tracks: vec![Track::new("a", 7), Track::new("b", 5), Track::new("c", 3)] };
        s.normalise();
        assert_eq!(s.cycle_steps(), 105);
    }

    #[test]
    fn quantise_picks_nearest_step_and_wraps() {
        assert_eq!(quantise(10, 0.49, 16), 10);
        assert_eq!(quantise(10, 0.51, 16), 11);
        assert_eq!(quantise(15, 0.9, 16), 0);
        assert_eq!(quantise(5, 0.0, 0), 0);
    }

    #[test]
    fn mods_over_lets_the_override_win_per_field() {
        let base = Mods { pitch: Some(0.5), drive: Some(2.0), ..Default::default() };
        let top = Mods { pitch: Some(2.0), crush: Some(8), ..Default::default() };
        let m = base.over(&top);
        assert_eq!(m.pitch, Some(2.0));
        assert_eq!(m.drive, Some(2.0));
        assert_eq!(m.crush, Some(8));
        assert_eq!(m.rev, None);
    }

    #[test]
    fn slot_for_note_uses_map_then_gm_default() {
        let mut t = Track::new("k", 16);
        assert_eq!(t.slot_for_note(36), Some(0));
        assert_eq!(t.slot_for_note(49), Some(8));
        assert_eq!(t.slot_for_note(60), None);
        t.keys.insert(60, 5);
        assert_eq!(t.slot_for_note(60), Some(5));
        assert_eq!(t.slot_for_note(36), None, "a custom map replaces the default entirely");
    }
}
