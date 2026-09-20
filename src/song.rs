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
//! A STRING voice (kit.rs's synth:string) takes a NOTE and can be articulated: `note` picks
//! the pitch, `damp` is the palm on the strings, `slide_ms` moves to the note without picking
//! it again, and `decay_ms` is the note ending. A string is monophonic -- one neck -- so a
//! hit on a slot that is already sounding takes it over.
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

use crate::kit::{Kit, Source};
use crate::string::Ks;
use crate::wub::WubState;
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
    /// force the hit to fade to -60 dB by this many ms, regardless of sample length. On a
    /// string voice this is the note ending: the gate, and the difference between eighth
    /// notes and DRIVING eighth notes.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub decay_ms: Option<f32>,
    /// MIDI note for a string voice: 28 = E1, a bass's low string. A sampled or wub voice
    /// ignores this; `pitch` is its equivalent, and on a string `pitch` is a ratio applied
    /// ON TOP of the note.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub note: Option<u8>,
    /// how fast a string voice loses its highs, 0..0.9. High is the heel of the hand resting
    /// on the strings -- a palm mute; with a low `vel` it is a ghost note.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub damp: Option<f32>,
    /// slide to this hit's note over this many ms rather than plucking it, when the string is
    /// already sounding. The turnaround slide, and it needs no note-off to work.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub slide_ms: Option<f32>,
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
            note: other.note.or(self.note),
            damp: other.damp.or(self.damp),
            slide_ms: other.slide_ms.or(self.slide_ms),
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
    /// Layer for a game or a live set: 0 always plays, 1..3 come in as the danger rises.
    /// Purely a tag here; the game (or whoever drives mute) reads it.
    #[serde(default)]
    pub intensity: u8,
}

fn default_pads() -> [usize; 4] { [0, 3, 1, 8] }

impl Track {
    pub fn new(name: &str, len: usize) -> Track {
        let len = len.max(1);
        Track { name: name.into(), len, pads: default_pads(), keys: BTreeMap::new(),
                cells: vec![Vec::new(); len], mute: false, intensity: 0 }
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

/// The project rate for a song that does not name one. Everything mixes at the project rate
/// and samples resample to it once, at kit load.
pub const DEFAULT_RATE: u32 = 48000;

fn default_rate() -> u32 { DEFAULT_RATE }

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Song {
    pub bpm: f32,
    /// Everything mixes here: the kit is loaded FOR this rate, so `Kit::rate` equals it.
    /// A song written before the field existed is 44.1 kHz by history, and says so.
    #[serde(default = "default_rate")]
    pub rate: u32,
    #[serde(default)]
    pub tracks: Vec<Track>,
}

impl Song {
    pub fn empty(bpm: f32) -> Song {
        Song { bpm, rate: DEFAULT_RATE, tracks: vec![Track::new("drums", 16)] }
    }
    pub fn normalise(&mut self) {
        if !(20.0..=300.0).contains(&self.bpm) { self.bpm = 120.0; }
        if !(8000..=192_000).contains(&self.rate) { self.rate = DEFAULT_RATE; }
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

/// How a sounding hit is producing samples. One variant per `kit::Source` kind that reaches
/// the mixer -- a sample and a kick are both buffers by the time they get here, so they
/// share one.
enum Playing {
    /// Reading `kit.voices[slot].mono` at a fractional cursor.
    Buffer {
        pos: f32,   // fractional source cursor
        step: f32,  // cursor advance per sample (pitch)
        rev: bool,
    },
    /// A wub being synthesised per hit. It applies its own drive/crush/decay from its
    /// parameters, so the instance-level ones are not applied on top.
    Wub(WubState),
    /// Ringing `Transport::strings[slot]`. The string lives in the Transport rather than here
    /// because it owns a delay line -- one per slot, allocated once, since a string is
    /// monophonic anyway and nothing may allocate on the audio thread.
    String,
}

/// A sounding hit with its mods resolved into per-instance state.
struct Instance {
    play: Playing,
    slot: usize,
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

/// Transport master gain before the soft knee. Baked into song.txt so the game matches.
pub const DEFAULT_MASTER: f32 = 2.0;

/// Owns the kit and the song inside the audio callback. Produces KIT-RATE audio.
pub struct Transport {
    pub kit: Arc<Kit>,
    pub song: Song,
    pub playing: bool,
    pub global_step: u64,
    pos_in_step: usize,
    voices: Vec<Instance>,
    /// One string per kit slot, allocated up front. Slots backed by a sample or a wub simply
    /// never ring theirs.
    strings: Vec<Ks>,
    /// Hits due this step, reused every step. It exists only so `trigger_step` can read the
    /// song and then mutate self without allocating on the audio thread.
    scratch: Vec<Hit>,
    pub master: f32,
    /// per-slot glow 0..1 for any display, decays per mix
    pub glow: [f32; 9],
}

impl Transport {
    /// `kit` must have been loaded for `song.rate` -- `Kit::load(path, song.rate)`. The mixer
    /// reads `kit.rate` as the authority, and the two being equal is what makes that safe.
    pub fn new(kit: Arc<Kit>, song: Song) -> Transport {
        let strings = (0..9).map(|_| Ks::new(kit.rate)).collect();
        Transport { kit, song, playing: false, global_step: 0, pos_in_step: 0,
                    voices: Vec::with_capacity(32), strings, scratch: Vec::with_capacity(64),
                    master: DEFAULT_MASTER, glow: [0.0; 9] }
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
        // An Arc bump rather than a borrow of `self.kit`: it costs one atomic increment and
        // no allocation, and it frees `self.strings` and `self.voices` to be touched below.
        let kit = self.kit.clone();
        let v = match kit.voices.get(h.slot).and_then(|v| v.as_ref()) { Some(v) => v, None => return };
        let m = &h.mods;
        let sr = kit.rate as f32;

        // Common to every kind. (vel/127)^2: perceived loudness tracks power, linear
        // velocity feels top-heavy.
        let gain = (h.vel.clamp(1, 127) as f32 / 127.0).powi(2) * m.gain.unwrap_or(1.0);
        let drive = m.drive.filter(|d| *d > 0.0);
        let crush = m.crush.filter(|b| (1..16).contains(b)).map(|b| (1u32 << b) as f32);
        let decay_k = m.decay_ms.filter(|d| *d > 0.0)
            .map(|d| (-(1000.0f32.ln()) / (sr * d / 1000.0)).exp());

        let play = match &v.source {
            // A string is played, not replayed: it takes a note per hit and rings on in
            // `self.strings[slot]` rather than being resynthesised as a one-shot.
            Source::String(p) => {
                // `pitch` is a ratio on top of the note rather than a replacement for it, so
                // the TUNE knob still detunes a string the way it detunes a sample.
                let f0 = crate::string::note_hz(m.note.unwrap_or(p.note) as f32)
                    * m.pitch.filter(|x| *x > 0.0).unwrap_or(1.0);
                let damp = m.damp.unwrap_or(p.damp);
                let sounding = self.voices.iter()
                    .any(|x| x.slot == h.slot && matches!(x.play, Playing::String));
                let ks = &mut self.strings[h.slot];
                match m.slide_ms.filter(|x| *x > 0.0) {
                    // A slide on a ringing string is the fretting hand moving: the delay line
                    // changes length and the string is never picked again. With nothing
                    // sounding there is nothing to slide from, so it is an ordinary note.
                    Some(ms) if sounding => { ks.glide_to(f0, ms); ks.set_damp(damp); }
                    _ => ks.pluck(f0, h.vel as f32 / 127.0, p, damp),
                }
                // One neck: a second note on this slot takes the string over rather than
                // stacking a second copy of it.
                self.voices.retain(|x| x.slot != h.slot);
                Playing::String
            }
            // A wub is synthesised: per-hit drive/crush override the kit's, pitch moves the
            // note, decay_ms the release, velocity the gain. The wobble follows the bpm.
            Source::Wub(p) => {
                let mut p = p.clone();
                if m.drive.is_some() { p.drive = m.drive; }
                if m.crush.is_some() { p.crush = m.crush; }
                Playing::Wub(WubState::new(&p, kit.rate,
                    m.pitch.filter(|p| *p > 0.0).unwrap_or(1.0), self.song.bpm, m.decay_ms))
            }
            // A sample and a kick are both just buffers by now.
            Source::Sample { .. } | Source::Kick(_) => {
                let rev = m.rev.unwrap_or(false);
                let len = v.mono.len() as f32;
                Playing::Buffer {
                    pos: if rev { (len - 2.0).max(0.0) } else { 0.0 },
                    step: m.pitch.filter(|p| *p > 0.0).unwrap_or(1.0),
                    rev,
                }
            }
        };

        if self.voices.len() >= 32 { self.voices.remove(0); }
        self.voices.push(Instance { play, slot: h.slot, gain, drive, crush, decay_k, env: 1.0 });
        self.glow[h.slot] = 1.0;
    }

    fn trigger_step(&mut self) {
        let gs = self.global_step;
        // Collect first, because triggering borrows self mutably. `scratch` is swapped out
        // and back rather than allocated: `take` leaves an empty Vec behind, which costs
        // nothing, and the buffer itself is reused every step.
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();
        for t in self.song.tracks.iter().filter(|t| !t.mute && t.len > 0) {
            scratch.extend_from_slice(&t.cells[(gs % t.len as u64) as usize]);
        }
        for h in &scratch { self.start(h); }
        self.scratch = scratch;
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
        let strings = &mut self.strings;
        self.voices.retain_mut(|v| match &mut v.play {
            Playing::String => {
                let ks = match strings.get_mut(v.slot) { Some(k) => k, None => return false };
                let norm = v.drive.map(|d| d.tanh());
                for o in out.iter_mut() {
                    if ks.done() { return false; }
                    let mut s = ks.next() * v.gain * v.env;
                    if let Some(lv) = v.crush { s = (s * lv).round() / lv; }
                    if let (Some(d), Some(n)) = (v.drive, norm) { s = (s * d).tanh() / n; }
                    *o += s;
                    if let Some(k) = v.decay_k { v.env *= k; if v.env < 0.001 { return false; } }
                }
                !ks.done()
            }
            // A wub carries its own drive, crush and decay in its parameters, so the
            // instance-level ones are deliberately NOT applied a second time here.
            Playing::Wub(st) => {
                for o in out.iter_mut() {
                    if st.done() { return false; }
                    *o += st.next() * v.gain;
                }
                !st.done()
            }
            Playing::Buffer { pos, step, rev } => {
                let src = match kit.voices.get(v.slot).and_then(|s| s.as_ref()) { Some(s) => &s.mono, None => return false };
                let last = src.len().saturating_sub(1);
                if last == 0 { return false; }
                let norm = v.drive.map(|d| d.tanh());
                for o in out.iter_mut() {
                    let i = *pos as usize;
                    if i >= last { return false; }
                    let f = *pos - i as f32;
                    let mut s = (src[i] * (1.0 - f) + src[i + 1] * f) * v.gain * v.env;
                    if let Some(lv) = v.crush { s = (s * lv).round() / lv; }
                    if let (Some(d), Some(n)) = (v.drive, norm) { s = (s * d).tanh() / n; }
                    *o += s;
                    if *rev { if *pos < *step { return false; } *pos -= *step; } else { *pos += *step; }
                    if let Some(k) = v.decay_k { v.env *= k; if v.env < 0.001 { return false; } }
                }
                true
            }
        });
    }

    /// Offline render of `cycles` full polymeter cycles (or `steps` if given) to kit-rate
    /// mono, at `master` gain -- the same knob the live transport runs at, so a render
    /// matches what was being played rather than whatever the default happened to be.
    pub fn render(kit: Arc<Kit>, song: Song, steps: usize, master: f32) -> Vec<f32> {
        let mut t = Transport::new(kit, song);
        t.master = master.max(0.0);
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
        // Long enough for the last hit to ring out: a second covers any drum or wub, but a
        // string is still sounding well past that, so give a kit with one the longer tail.
        let strings = t.kit.voices.iter().flatten().any(|v| matches!(v.source, Source::String(_)));
        let mut tail = vec![0.0f32; (t.kit.rate as f32 * if strings { 2.5 } else { 1.0 }) as usize];
        t.fill(&mut tail);
        out.extend_from_slice(&tail);
        out
    }
}

/// Project-rate → device-rate shim with a fractional cursor carried across calls (no clicks
/// at callback boundaries).
///
/// This is the FALLBACK, not the normal path: the project rate is meant to be the device's
/// native rate, and then this is never constructed. It exists for the device that will not
/// open at the project's rate.
pub struct Resampler {
    ratio: f32,      // project_rate / dev_rate
    buf: Vec<f32>,   // project-rate samples not yet consumed
    pos: f32,
}

/// Capacity of `Resampler::buf`, committed at construction so `run` never reallocates on the
/// audio thread. One callback needs `out.len() * ratio` samples; this covers a 8192-frame
/// device buffer resampling up from any sane rate, which is far past anything WASAPI hands us.
const RESAMPLER_CAP: usize = 16384;

impl Resampler {
    pub fn new(project_rate: u32, dev_rate: u32) -> Resampler {
        Resampler { ratio: project_rate as f32 / dev_rate as f32,
                    buf: Vec::with_capacity(RESAMPLER_CAP), pos: 0.0 }
    }
    /// Produce `out.len()` device-rate samples, pulling project-rate audio via `pull` as needed.
    pub fn run<F: FnMut(&mut [f32])>(&mut self, out: &mut [f32], mut pull: F) {
        let need = (out.len() as f32 * self.ratio) as usize + 2;
        debug_assert!((self.pos as usize) + need + 1024 <= RESAMPLER_CAP,
                      "resampler buffer would grow past its committed capacity and allocate \
                       on the audio thread: need {need}, cap {RESAMPLER_CAP}");
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
        let mut s = Song { bpm: 120.0, rate: DEFAULT_RATE,
                           tracks: vec![Track::new("a", 7), Track::new("b", 5), Track::new("c", 3)] };
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

    /// The point of Phase 0's allocation work. A Vec growing inside the callback is a lock
    /// and maybe a syscall on a thread with a ~10 ms deadline; it never fails loudly, it
    /// fails as a click under load on someone else's machine. All three source kinds are
    /// sounding at once here, because each one takes a different path through `mix`.
    #[test]
    fn filling_the_audio_buffer_allocates_nothing() {
        use crate::kit::{KickParams, Source, Voice as KitVoice};
        use crate::wub::WubParams;

        let voice = |source| Some(KitVoice {
            label: "v".into(), color: [0, 0, 0], mono: vec![0.1; 4096], source,
        });
        let mut voices: [Option<KitVoice>; 9] = Default::default();
        voices[0] = voice(Source::Kick(KickParams::default()));
        voices[1] = voice(Source::Sample { path: "unused.wav".into() });
        voices[2] = voice(Source::Wub(WubParams::default()));
        voices[3] = voice(Source::String(crate::string::Params::default()));

        let kit = Arc::new(Kit { name: "t".into(), rate: 48000, voices });
        let mut song = Song::empty(120.0);
        song.rate = 48000;
        song.tracks[0].len = 4;
        song.tracks[0].cells = (0..4).map(|s| vec![Hit { slot: s, vel: 100, mods: Mods::default() }]).collect();

        let mut t = Transport::new(kit, song);
        t.playing = true;
        let mut buf = vec![0.0f32; 512];
        // Warm up outside the guard: the scratch and voice vectors are allowed to reach
        // their working size once. What must not happen is growth in the steady state.
        for _ in 0..16 { t.fill(&mut buf); }

        let before = crate::noalloc::violations();
        crate::noalloc::forbidden(|| { for _ in 0..400 { t.fill(&mut buf); } });
        assert_eq!(crate::noalloc::violations(), before,
                   "Transport::fill allocated on the audio thread");
    }

    #[test]
    fn a_song_without_a_rate_reads_as_the_default_and_one_with_it_is_kept() {
        let s: Song = serde_json::from_str(r#"{"bpm":120.0,"tracks":[]}"#).unwrap();
        assert_eq!(s.rate, DEFAULT_RATE);
        let s: Song = serde_json::from_str(r#"{"bpm":120.0,"rate":44100,"tracks":[]}"#).unwrap();
        assert_eq!(s.rate, 44100);
        let mut s: Song = serde_json::from_str(r#"{"bpm":120.0,"rate":7,"tracks":[]}"#).unwrap();
        s.normalise();
        assert_eq!(s.rate, DEFAULT_RATE, "an absurd rate falls back rather than dividing by it");
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
