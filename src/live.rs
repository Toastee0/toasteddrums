//! live — play the kit from the pads, in real time.
//!
//! Ties the three halves together: `pads` (serial hits) → voice triggers → `audio` (winmm
//! output), with a terminal UI so you can see what the pads are actually doing while you
//! play them.
//!
//! Free play, not the sequencer: every hit fires immediately. Recording pads into a pattern
//! is a separate job and belongs after this feels right to play.

use crate::audio::Out;
use crate::kit::Kit;
use crate::pads::{Msg, Pads};

/// One sounding sample. Drums are one-shots, so this is just a cursor and a gain.
struct Playing {
    slot: usize,
    pos: usize,
    gain: f32,
}

/// Rendered meter state per pad, purely for the UI.
struct Meter {
    vel: u8,
    decay: f32,
}

pub struct Live {
    kit: Kit,
    /// pad index → kit slot. Pads and voices are different things: four plates onto nine
    /// voices, so this is a real mapping and not an identity.
    map: Vec<usize>,
    playing: Vec<Playing>,
    meters: Vec<Meter>,
    names: Vec<String>,
    /// Resting baseline per pad, from `get`. Shown in the UI so a mis-calibrated pad is
    /// visible before you wonder why it will not trigger.
    bases: Vec<u32>,
    hits: u64,
    /// The device's peak-search window, mirrored here so the UI can report honest total
    /// latency rather than just the part the host controls.
    hold_ms: u32,
    /// Rolling log of the last few strikes, newest last.
    log: Vec<String>,
}

/// Defaults chosen for the physical layout: the two floor plates are feet (kick, hat),
/// the two wall plates are hands (snare, crash).
pub const DEFAULT_MAP: [usize; 4] = [0, 3, 1, 8];

impl Live {
    pub fn new(kit: Kit, map: Vec<usize>) -> Live {
        let n = map.len();
        Live {
            kit,
            map,
            playing: Vec::with_capacity(32),
            meters: (0..n).map(|_| Meter { vel: 0, decay: 0.0 }).collect(),
            names: (0..n).map(|i| format!("pad{i}")).collect(),
            bases: vec![0; n],
            hits: 0,
            hold_ms: 8,
            log: Vec::new(),
        }
    }

    /// Records a pad's real name and resting baseline, learned from `get` at startup.
    pub fn set_name(&mut self, pad: usize, name: &str, base: u32) {
        if pad < self.names.len() {
            self.names[pad] = name.to_string();
            self.bases[pad] = base;
        }
    }

    fn trigger(&mut self, pad: usize, name: &str, vel: u8) {
        if pad >= self.map.len() { return; }
        if pad < self.names.len() { self.names[pad] = name.to_string(); }
        let slot = self.map[pad];
        let label = match self.kit.voices.get(slot).and_then(|v| v.as_ref()) {
            Some(v) => v.label.clone(),
            // A pad mapped to an empty slot is a config mistake, not a crash. Say so in the
            // log rather than silently dropping the beat.
            None => {
                self.log.push(format!("{name} -> slot {slot} is empty"));
                return;
            }
        };

        // Velocity is 1-127 from the device, already scaled by that pad's gain. Square it
        // for loudness: perceived level tracks power, so linear velocity feels top-heavy.
        let g = (vel as f32 / 127.0).powi(2);

        // Cheap voice stealing: drums retrigger constantly and an unbounded vector would
        // grow without limit under a roll.
        if self.playing.len() >= 24 { self.playing.remove(0); }
        self.playing.push(Playing { slot, pos: 0, gain: g });

        self.hits += 1;
        self.meters[pad].vel = vel;
        self.meters[pad].decay = 1.0;
        self.log.push(format!("{name:<4} {label:<8} vel {vel:>3}"));
        if self.log.len() > 8 { self.log.remove(0); }
    }

    /// Sums every sounding voice into `buf`. Finished voices are dropped.
    fn mix(&mut self, buf: &mut [f32]) {
        let voices = &self.kit.voices;
        self.playing.retain_mut(|p| {
            let v = match voices.get(p.slot).and_then(|v| v.as_ref()) {
                Some(v) => v,
                None => return false,
            };
            let n = buf.len().min(v.mono.len().saturating_sub(p.pos));
            for i in 0..n {
                buf[i] += v.mono[p.pos + i] * p.gain;
            }
            p.pos += n;
            p.pos < v.mono.len()
        });
    }

    fn draw(&self, latency_ms: f32, queued: usize) {
        // Home the cursor and repaint rather than scrolling, so the meters sit still.
        let mut s = String::with_capacity(1024);
        s.push_str("\x1b[H\x1b[2J");
        // Report the whole chain, not just the audio queue. The device's peak-search window
        // is a latency floor too, and quoting only the part I control would flatter it.
        s.push_str(&format!(
            "ToastedDrums live — kit '{}' @ {} Hz   latency ~{:.0} ms \
             (audio {:.1} + pad hold {} + link ~2)   hits {}\n\n",
            self.kit.name, self.kit.rate,
            latency_ms + self.hold_ms as f32 + 2.0, latency_ms, self.hold_ms, self.hits));
        let _ = queued;

        for (i, m) in self.meters.iter().enumerate() {
            let slot = self.map[i];
            let label = self.kit.voices.get(slot).and_then(|v| v.as_ref())
                .map(|v| v.label.as_str()).unwrap_or("(empty)");
            let lit = (m.decay * 28.0) as usize;
            let bar: String = (0..28).map(|k| if k < lit { '#' } else { '.' }).collect();
            s.push_str(&format!("  {:<4} → {:<8} {bar} {:>3}   base {:>4}\n",
                                self.names[i], label, m.vel, self.bases[i]));
        }

        s.push('\n');
        for line in &self.log { s.push_str(&format!("  {line}\n")); }
        s.push_str("\n  Ctrl-C to stop\n");
        print!("{s}");
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
}

pub fn run(kit: Kit, port: &str, baud: u32, map: Vec<usize>) -> Result<(), String> {
    let rate = kit.rate;
    let mut live = Live::new(kit, map);
    let mut out = Out::open(rate)?;
    let mut pads = Pads::open(port, baud)?;
    pads.start("toasteddrums")?;
    // Hits only: raw windows and traces are calibration tools and would just burn
    // bandwidth and parse time in the play loop.
    pads.send("mode hits")?;

    // `hold` is the device's peak-search window: it waits this long after onset to find the
    // deepest point before reporting, so it is a hard latency floor on every hit. The
    // default 12 ms is generous — measured strikes reach bottom in roughly 6-8 ms at the
    // 7-22 %/ms slopes these plates produce — so 8 keeps essentially all of the velocity
    // range and gives 4 ms back. Live setting, no reboot.
    pads.send("set hold 8")?;

    // Re-calibrate every session. Baselines are learned in the second after boot, and they
    // shift a lot with whatever is near the plates — the same pad has measured 74 and 305
    // on different days. Inheriting a stale baseline from whenever the device last booted
    // is how pads end up silently unable to reach their threshold.
    eprintln!("calibrating — hands off the pads");
    pads.send("cal")?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2500);
    let mut baselines: Vec<String> = Vec::new();
    while std::time::Instant::now() < deadline {
        for m in pads.poll()? {
            match m {
                Msg::Notice(n) if n.starts_with("cal ") => baselines.push(n),
                Msg::Reply(r) if r.text.starts_with("cal") => {
                    // calibration finished; drain nothing further
                }
                _ => {}
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // Names and baselines come out of the `! cal` lines we were already waiting on — no
    // extra query, no extra wait. `! cal P33 base=524 min=493 max=557 spread=64`.
    // Ordering matches the device's pad indices, which is the same order it reports hits in.
    for (i, b) in baselines.iter().enumerate() {
        eprintln!("  {b}");
        let f: Vec<&str> = b.split_whitespace().collect();
        if f.len() >= 3 {
            let base = f[2].strip_prefix("base=").and_then(|v| v.parse().ok()).unwrap_or(0);
            live.set_name(i, f[1], base);
        }
    }

    let mut last_draw = std::time::Instant::now();
    loop {
        for m in pads.poll()? {
            match m {
                Msg::Hit(h) => live.trigger(h.pad as usize, &h.name, h.vel),
                Msg::Reply(r) if !r.ok => {
                    live.log.push(format!("err {} {}", r.code.unwrap_or_default(), r.text));
                }
                _ => {}
            }
        }

        out.pump(|buf| live.mix(buf))?;

        if last_draw.elapsed().as_millis() >= 40 {
            let dt = last_draw.elapsed().as_secs_f32();
            for m in live.meters.iter_mut() {
                m.decay = (m.decay - dt * 3.0).max(0.0);
            }
            let queued = out.queued();
            live.draw(queued as f32 * 1000.0 / rate as f32, queued);
            last_draw = std::time::Instant::now();
        }

        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}
