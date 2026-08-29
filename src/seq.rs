//! 16-step pattern sequencer + mixer. One pattern = 9 voices × 16 steps of velocity (0 = rest).
//! Pattern file: 9 lines of 16 chars, `.`=rest, `x`=hit, `X`=accent, `1`-`9`=velocity/9.

use crate::kit::Kit;

#[derive(Clone, Debug)]
pub struct Pattern {
    pub bpm: f32,
    pub steps: [[u8; 16]; 9], // velocity 0..=9
}

impl Pattern {
    pub fn parse(text: &str) -> Result<Pattern, String> {
        let mut p = Pattern { bpm: 120.0, steps: [[0; 16]; 9] };
        let mut row = 0;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            if let Some(b) = line.strip_prefix("bpm ") {
                p.bpm = b.trim().parse().map_err(|_| format!("bad bpm {b}"))?;
                continue;
            }
            if row >= 9 { return Err("more than 9 voice rows".into()); }
            let body = line.split('|').next().unwrap_or("");
            let cells: Vec<char> = body.chars().filter(|c| !c.is_whitespace()).collect();
            if cells.len() != 16 { return Err(format!("row {row}: need 16 steps, got {}", cells.len())); }
            for (i, c) in cells.iter().enumerate() {
                p.steps[row][i] = match c {
                    '.' | '-' => 0, 'x' => 6, 'X' => 9,
                    '1'..='9' => *c as u8 - b'0',
                    _ => return Err(format!("row {row}: bad cell {c}")),
                };
            }
            row += 1;
        }
        Ok(p)
    }

    pub fn samples_per_step(&self, rate: u32) -> usize {
        (rate as f32 * 60.0 / self.bpm / 4.0) as usize
    }
}

/// A live voice instance being mixed.
struct Playing { voice: usize, pos: usize, gain: f32 }

/// Renders the pattern in real time chunks: call `next_step()` on each step boundary to
/// trigger hits, `mix(out)` to fill audio. Keeps the last triggered velocities for the display.
pub struct Engine<'a> {
    pub kit: &'a Kit,
    pub pattern: Pattern,
    pub step: usize,
    playing: Vec<Playing>,
    /// per-voice decaying "glow" 0..1 for the visualizer
    pub glow: [f32; 9],
}

impl<'a> Engine<'a> {
    pub fn new(kit: &'a Kit, pattern: Pattern) -> Self {
        Engine { kit, pattern, step: 0, playing: Vec::new(), glow: [0.0; 9] }
    }

    /// Advance to the next step, triggering voices. Returns the step index just triggered.
    pub fn next_step(&mut self) -> usize {
        let s = self.step;
        for v in 0..9 {
            let vel = self.pattern.steps[v][s];
            if vel > 0 && self.kit.voices[v].is_some() {
                let gain = vel as f32 / 9.0;
                // retrigger: drop any earlier instance of this voice (toy keyboards are monophonic per pad)
                self.playing.retain(|p| p.voice != v);
                self.playing.push(Playing { voice: v, pos: 0, gain });
                self.glow[v] = gain;
            }
        }
        self.step = (s + 1) % 16;
        s
    }

    /// Mix `out.len()` mono samples (additive, clears first).
    pub fn mix(&mut self, out: &mut [f32]) {
        out.iter_mut().for_each(|o| *o = 0.0);
        for p in &mut self.playing {
            let src = &self.kit.voices[p.voice].as_ref().unwrap().mono;
            let n = out.len().min(src.len().saturating_sub(p.pos));
            for i in 0..n { out[i] += src[p.pos + i] * p.gain; }
            p.pos += n;
        }
        self.playing.retain(|p| p.pos < self.kit.voices[p.voice].as_ref().unwrap().mono.len());
        // soft clip: tanh-ish knee so 9 accented voices don't square off
        for o in out.iter_mut() { if o.abs() > 0.8 { *o = o.signum() * (0.8 + (o.abs() - 0.8).tanh() * 0.2); } }
        // glow decays over ~ one step at 120bpm regardless of chunk size
        let decay = (-(out.len() as f32) / (self.kit.rate as f32 * 0.12)).exp();
        for g in &mut self.glow { *g *= decay; }
    }

    /// Offline render of `bars` bars to mono f32 — the bench path (no audio device needed).
    pub fn render(&mut self, bars: usize) -> Vec<f32> {
        let sps = self.pattern.samples_per_step(self.kit.rate);
        let mut out = vec![0.0f32; sps * 16 * bars];
        let mut chunk = vec![0.0f32; sps];
        for i in 0..16 * bars {
            self.next_step();
            self.mix(&mut chunk);
            out[i * sps..(i + 1) * sps].copy_from_slice(&chunk);
        }
        // tail
        let mut tail = vec![0.0f32; sps * 4];
        self.mix(&mut tail);
        out.extend_from_slice(&tail);
        out
    }
}
