//! live — play the kit from the pads, in real time.
//!
//! Ties the three halves together: `pads` (serial hits) → voice triggers → `audio` (winmm
//! output), with a terminal UI so you can see what the pads are doing while you play them.
//!
//! THREADING, and why it is not optional: the audio refill runs on its own thread and does
//! nothing else. It used to share a loop with the terminal redraw, and a Windows console
//! repaint can block for tens of milliseconds — far longer than the ~12 ms of audio queued —
//! so the buffers ran dry and the kit came out as a crackling stutter rather than drums.
//! The main thread reads serial and draws; it sends triggers down a channel and never
//! touches the audio device.
//!
//! Free play, not the sequencer: every hit fires immediately. Recording pads into a pattern
//! is a separate job and belongs after this feels right to play.

use crate::audio::Out;
use crate::kit::Kit;
use crate::pads::{Learn, Msg, Pads};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};

/// A strike, on its way to the audio thread. Deliberately tiny and Send: no locks on the
/// audio path, just a queue.
struct Trigger {
    slot: usize,
    gain: f32,
}

/// One sounding sample. Drums are one-shots, so this is just a cursor and a gain. The
/// cursor is fractional, in SOURCE frames: it advances by `Mixer::ratio` per output frame,
/// which is what pitch-corrects a 44.1 kHz kit onto a 48 kHz device.
struct Playing {
    slot: usize,
    pos: f32,
    gain: f32,
}

/// Owns the samples and the sounding voices. Lives entirely on the audio thread.
struct Mixer {
    voices: Vec<Option<Vec<f32>>>,
    playing: Vec<Playing>,
    /// kit_rate / device_rate. 1.0 when they match; 0.919 for a 44.1 k kit on a 48 k device.
    ratio: f32,
    /// Applied to the summed mix, BEFORE the soft clip. The level curve is (vel/127)², so a
    /// typical hit at vel ~79 lands at only ~39% amplitude and the kit plays quiet; 2.0 puts
    /// normal playing where it should be and lets the tanh knee limit hard hits instead of
    /// leaving headroom nobody uses.
    master: f32,
}

/// Default master gain. Overridable per run: `live <kit> [port] [map] [gain]`.
pub const DEFAULT_MASTER: f32 = 2.0;

impl Mixer {
    fn start(&mut self, t: Trigger) {
        // Cheap voice stealing: drums retrigger constantly and an unbounded vector would
        // grow without limit under a roll.
        if self.playing.len() >= 24 { self.playing.remove(0); }
        self.playing.push(Playing { slot: t.slot, pos: 0.0, gain: t.gain });
    }

    fn mix(&mut self, buf: &mut [f32]) {
        let voices = &self.voices;
        let ratio = self.ratio;
        self.playing.retain_mut(|p| {
            let v = match voices.get(p.slot).and_then(|v| v.as_ref()) {
                Some(v) => v,
                None => return false,
            };
            // Linear-interpolating read at a fractional source position. For one-shot
            // drum hits this is plenty; it is the whole of the resampler and needs no crate.
            let last = v.len().saturating_sub(1);
            for o in buf.iter_mut() {
                let i = p.pos as usize;
                if i >= last { break; }
                let frac = p.pos - i as f32;
                let s = v[i] * (1.0 - frac) + v[i + 1] * frac;
                *o += s * p.gain;
                p.pos += ratio;
            }
            (p.pos as usize) < last
        });

        let master = self.master;
        for o in buf.iter_mut() { *o *= master; }

        // Same soft clip as the offline mixer (seq.rs), so live and `render` sound alike.
        // This is NOT optional: hit samples already peak near 1.0 and a full-velocity gain
        // is 1.0, so single strikes sit on the limit and overlapping tails go over it. The
        // hard clip in audio.rs squares the waveform off, which on a kick is heard as a
        // flatulent buzz, on a hat as tinny, and on a snare as fuzz. A tanh knee rounds the
        // peaks instead of shearing them.
        for o in buf.iter_mut() {
            if o.abs() > 0.8 {
                *o = o.signum() * (0.8 + (o.abs() - 0.8).tanh() * 0.2);
            }
        }
    }
}

/// Rendered meter state per pad, purely for the UI.
struct Meter {
    vel: u8,
    decay: f32,
}

struct Ui {
    kit_name: String,
    rate: u32,
    labels: Vec<String>,
    map: Vec<usize>,
    meters: Vec<Meter>,
    names: Vec<String>,
    bases: Vec<u32>,
    hits: u64,
    hold_ms: u32,
    log: Vec<String>,
}

/// Defaults chosen for the physical layout: the two floor plates are feet (kick, hat),
/// the two wall plates are hands (snare, crash).
pub const DEFAULT_MAP: [usize; 4] = [0, 3, 1, 8];

impl Ui {
    fn set_pad(&mut self, pad: usize, name: &str, base: u32) {
        if pad < self.names.len() {
            self.names[pad] = name.to_string();
            self.bases[pad] = base;
        }
    }

    fn note_hit(&mut self, pad: usize, name: &str, vel: u8, label: &str) {
        if pad < self.names.len() { self.names[pad] = name.to_string(); }
        self.hits += 1;
        self.meters[pad].vel = vel;
        self.meters[pad].decay = 1.0;
        self.log.push(format!("{name:<4} {label:<8} vel {vel:>3}"));
        if self.log.len() > 8 { self.log.remove(0); }
    }

    fn draw(&self, audio_ms: f32) {
        // Home the cursor and overwrite, padding each line, rather than clearing the whole
        // screen. A full clear-and-repaint is markedly more console I/O, and console I/O is
        // what wrecked the audio when these shared a thread — no reason to keep paying it.
        let mut s = String::with_capacity(1024);
        s.push_str("\x1b[H");
        s.push_str(&format!(
            "ToastedDrums live — kit '{}' @ {} Hz   latency ~{:.0} ms \
             (audio {:.1} + pad hold {} + link ~2)   hits {}\x1b[K\n\x1b[K\n",
            self.kit_name, self.rate,
            audio_ms + self.hold_ms as f32 + 2.0, audio_ms, self.hold_ms, self.hits));

        for (i, m) in self.meters.iter().enumerate() {
            let label = self.labels.get(self.map[i]).map(String::as_str).unwrap_or("(empty)");
            let lit = (m.decay * 28.0) as usize;
            let bar: String = (0..28).map(|k| if k < lit { '#' } else { '.' }).collect();
            s.push_str(&format!("  {:<4} → {:<8} {bar} {:>3}   base {:>4}\x1b[K\n",
                                self.names[i], label, m.vel, self.bases[i]));
        }

        s.push_str("\x1b[K\n");
        for line in &self.log { s.push_str(&format!("  {line}\x1b[K\n")); }
        s.push_str("\x1b[J\n  Ctrl-C to stop\n");
        print!("{s}");
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
}

pub fn run(kit: Kit, port: &str, baud: u32, map: Vec<usize>, master: f32) -> Result<(), String> {
    let rate = kit.rate;
    let n = map.len();

    // Split the kit: samples go into the audio callback, labels stay here for the UI.
    let voices: Vec<Option<Vec<f32>>> =
        kit.voices.iter().map(|v| v.as_ref().map(|v| v.mono.clone())).collect();
    let labels: Vec<String> = kit.voices.iter()
        .map(|v| v.as_ref().map(|v| v.label.clone()).unwrap_or_else(|| "(empty)".into()))
        .collect();

    // Open at the device's native rate and pitch-correct the kit onto it — never ask the
    // device for the kit's rate (see audio.rs). The ratio has to be known before the
    // closure is built, hence the separate query.
    let dev_rate = crate::audio::device_rate()?;
    let ratio = rate as f32 / dev_rate as f32;

    // cpal owns the audio thread and calls this closure per buffer. It drains pending
    // triggers, then mixes — no printing, no serial, nothing that can block. The Mixer and
    // the channel receiver move in and live on that thread for the life of the stream.
    let (tx, rx): (Sender<Trigger>, Receiver<Trigger>) = channel();
    let mut mix = Mixer { voices, playing: Vec::with_capacity(32), ratio, master };
    let out = Out::open(move |buf| {
        loop {
            match rx.try_recv() {
                Ok(t) => mix.start(t),
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        mix.mix(buf);
    })?;

    let mut ui = Ui {
        kit_name: kit.name.clone(),
        rate,
        labels,
        map,
        meters: (0..n).map(|_| Meter { vel: 0, decay: 0.0 }).collect(),
        names: (0..n).map(|i| format!("pad{i}")).collect(),
        bases: vec![0; n],
        hits: 0,
        hold_ms: 8,
        log: Vec::new(),
    };

    // The session, calibration and learn phase are the shared implementation in pads.rs --
    // the MCP server runs the identical sequence. This function only owns the terminal
    // narration around it.
    let mut pads = Pads::open(port, baud)?;
    pads.session_start("toasteddrums")?;

    // Re-calibrate every session. The baseline is static once measured, so it matters that
    // it is measured now, against the room as it is, rather than inherited from whenever
    // the device last booted.
    eprintln!("calibrating — hands off the pads");
    let cal = pads.calibrate(std::time::Duration::from_millis(2500))?;
    for c in &cal {
        eprintln!("  {}", c.describe());
        ui.set_pad(c.pad, &c.name, c.base);
    }

    // ---- learn phase: three taps per pad set that pad's thresh and gain -----------------
    // Thresholds are absolute counts and a pad's swing depends entirely on how it is
    // mounted (carpet vs. wooden blocks changed the floor pads' swing from ~50 to 80+),
    // so carrying numbers between sessions is how pads end up pinned at 127 or silent.
    // Learn them every time instead. During learning the trigger sits at 2x the noise
    // floor so any genuine tap registers without the noise doing so.
    eprintln!("untouched state OK — tap each pad 3 times please");
    pads.set_learn_thresholds(&cal)?;
    let mut learn = Learn::new(&cal, std::time::Duration::from_secs(90));
    let mut last_report = std::time::Instant::now();
    while !learn.done() {
        for m in pads.poll()? {
            if let Msg::Hit(h) = m {
                if let Some((_, count)) = learn.feed(&h) {
                    eprintln!("  {:<4} tap {count}/3  drop {:.0} counts", h.name, h.depth);
                }
            }
        }
        if last_report.elapsed().as_secs() >= 10 {
            let waiting: Vec<String> = learn.pending().iter()
                .map(|&(i, c)| format!("{} ({c}/3)", ui.names[i])).collect();
            eprintln!("  still need: {}", waiting.join(", "));
            last_report = std::time::Instant::now();
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    eprintln!("learned:");
    for (i, r) in learn.results().iter().enumerate() {
        match r {
            // Nothing tapped: leave the learn threshold in place rather than inventing a
            // gain. The pad still triggers; its velocity will just be uncalibrated.
            None => eprintln!("  {:<4} no taps — keeping learn threshold, velocity uncalibrated", ui.names[i]),
            Some((thresh, gain)) => {
                pads.apply_learned(i, *thresh, *gain)?;
                eprintln!("  {:<4} → thresh {thresh:.0}  gain {gain:.0}", ui.names[i]);
            }
        }
    }
    // Drain the acks so they do not land in the play log.
    std::thread::sleep(std::time::Duration::from_millis(150));
    let _ = pads.poll()?;

    let audio_ms = out.latency_ms();
    print!("\x1b[2J");                                    // clear once; redraws overwrite

    let mut last_draw = std::time::Instant::now();
    loop {
        for m in pads.poll()? {
            match m {
                Msg::Hit(h) => {
                    let pad = h.pad as usize;
                    if pad >= ui.map.len() { continue; }
                    let slot = ui.map[pad];
                    // Velocity is 1-127 from the device, already scaled by that pad's gain.
                    // Square it for loudness: perceived level tracks power, so linear
                    // velocity feels top-heavy.
                    let gain = (h.vel as f32 / 127.0).powi(2);
                    let label = ui.labels.get(slot).cloned().unwrap_or_default();
                    if tx.send(Trigger { slot, gain }).is_err() {
                        return Err("audio thread stopped".into());
                    }
                    ui.note_hit(pad, &h.name, h.vel, &label);
                }
                Msg::Reply(r) if !r.ok => {
                    ui.log.push(format!("err {} {}", r.code.unwrap_or_default(), r.text));
                }
                _ => {}
            }
        }

        if last_draw.elapsed().as_millis() >= 50 {
            let dt = last_draw.elapsed().as_secs_f32();
            for m in ui.meters.iter_mut() { m.decay = (m.decay - dt * 3.0).max(0.0); }
            ui.draw(audio_ms);
            last_draw = std::time::Instant::now();
        }

        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}
