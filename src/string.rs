//! string — a plucked string as a delay line with a lossy feedback path (Karplus-Strong).
//!
//! WHY THIS AND NOT A SAMPLE: a bass has to play a different note every eighth, slide
//! between them, and shut up when the player's palm lands. A sample can do the first of
//! those by resampling, badly -- the whole instrument changes size with the pitch -- and
//! the other two not at all. A string is forty lines and costs about what the sample
//! reader costs per voice.
//!
//! THE LOOP: read the delay line, lose some highs in a one-pole, lose some level, write it
//! back. That is the whole physical model -- the string's round trip, and the bridge eating
//! the top end a little more each pass. Pitch is the loop's length; sustain is its gain;
//! tone is how much the one-pole eats.
//!
//! TUNING, and why the allpass is not optional. The loop delay has to be rate/f0 samples and
//! that is nowhere near a whole number: at 44.1 kHz a low E (41.2 Hz) wants 1070.4. Round it
//! and you are 0.4 samples short, which is 26 cents -- audibly flat against anything else
//! playing. The error grows as the note drops, so naive Karplus-Strong is out of tune
//! exactly where a bass lives. A first-order allpass supplies the fractional part. Its
//! neighbours in the loop have delays of their own (the damping one-pole has a group delay
//! of damp/(1-damp) samples at low frequency), so those come off the integer length before
//! the fraction is handed to the allpass. `tuning_lands_within_ten_cents` measures the
//! result rather than trusting this paragraph.
//!
//! WHAT MAKES IT A BASS GUITAR rather than a generic plucked thing is the excitation, not
//! the resonator, and the excitation is NOT noise. Textbook Karplus-Strong fills the line
//! with white noise, which is why it sounds like a banjo: white noise is flat across the
//! harmonics, and a real plucked string is not remotely flat.
//!
//! What a string actually starts from is its SHAPE at the moment of release -- a triangle,
//! apex at the pick point. Its harmonic amplitudes are sin(pi*h*pick)/h^2: the comb from the
//! pick position (every multiple of 1/pick has a node there and cannot be excited), times a
//! 1/h^2 rolloff. Both come free from drawing the triangle. Take the triangle away and you
//! keep only the comb, and at a bridge-ish pick=0.12 the comb ALONE puts the fundamental
//! 9 dB BELOW the fourth harmonic -- measured, before this was fixed. That is a fine banjo
//! and a useless bass. `the_fundamental_is_the_loudest_partial` is the regression test.
//!
//! Pick hardness (`tone`) then rides a little filtered noise on top, which is the plectrum
//! itself rather than the string: more of it, and brighter, for a pick than for a thumb.

/// Instrument parameters, fixed per kit voice (the `synth:string` tokens in a kit file).
#[derive(Clone, Debug)]
pub struct Params {
    /// MIDI note a hit plays when it does not name one. 28 = E1, a bass's low string.
    pub note: u8,
    /// pluck position as a fraction of the string, 0.02..0.5. 0.12 is picking near the
    /// bridge, 0.5 is over the middle -- which is the fattest and dullest, because the comb
    /// then reinforces the fundamental and nulls the even harmonics.
    pub pick: f32,
    /// pick hardness 0..1: how much bright noise the plectrum adds over the string's own
    /// shape. 1 is a plectrum, 0.2 is a thumb.
    pub tone: f32,
    /// feedback damping 0..0.9: higher is duller and loses its highs faster.
    pub damp: f32,
    /// time to -60 dB, in ms. Converted to a loop gain per note, so sustain is the same
    /// length across the register instead of shortening as the pitch rises.
    pub decay_ms: f32,
    /// output lowpass. There is nothing above ~5 k on a bass and leaving it in only gives
    /// the drive something to fizz on.
    pub lp_hz: f32,
}

impl Default for Params {
    fn default() -> Params {
        Params { note: 28, pick: 0.12, tone: 0.55, damp: 0.30, decay_ms: 1600.0, lp_hz: 4500.0 }
    }
}

/// MIDI note number (fractional, so a pitch ratio can bend it) to Hz. 69 = A440.
pub fn note_hz(note: f32) -> f32 { 440.0 * ((note - 69.0) / 12.0).exp2() }

const NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];

/// A MIDI note as a name: 28 -> "E1", 38 -> "D2". Octaves are numbered the way a bass
/// player numbers strings -- low E is E1 -- which is the MIDI convention with C-1 at 0.
pub fn note_name(note: u8) -> String {
    format!("{}{}", NAMES[note as usize % 12], note as i32 / 12 - 1)
}

/// The inverse: "D2", "F#1", "Bb2", "a1" -> a MIDI note. Case-insensitive, and both `#`
/// and `b` work, because a chord chart written in D uses whichever fits the key.
pub fn parse_note(s: &str) -> Option<u8> {
    let s = s.trim();
    // A bare number is already a MIDI note; that is what the JSON schema takes too.
    if let Ok(n) = s.parse::<u8>() { return (n < 128).then_some(n); }
    let mut c = s.chars();
    let letter = c.next()?.to_ascii_uppercase();
    let mut semi = match letter { 'C' => 0, 'D' => 2, 'E' => 4, 'F' => 5, 'G' => 7, 'A' => 9, 'B' => 11, _ => return None } as i32;
    let rest = c.as_str();
    let rest = match rest.chars().next() {
        Some('#') => { semi += 1; &rest[1..] }
        Some('b') => { semi -= 1; &rest[1..] }
        _ => rest,
    };
    let octave: i32 = rest.parse().ok()?;
    let n = (octave + 1) * 12 + semi;
    (0..128).contains(&n).then_some(n as u8)
}

/// The lowest note the delay line is sized for: below a 5-string's low B (30.9 Hz), with
/// room for a slide to overshoot into.
const MIN_HZ: f32 = 20.0;

/// One string. Allocated once per kit slot by the transport -- never on the audio thread.
pub struct Ks {
    buf: Vec<f32>,
    mask: usize,
    idx: usize,
    rate: f32,
    /// total loop delay in samples = rate / f0
    delay: f32,
    /// where a slide is heading, and how far the delay moves per output sample
    target: f32,
    glide: f32,
    damp: f32,
    lp: f32,
    ap_x1: f32,
    ap_y1: f32,
    loop_gain: f32,
    out_lp: f32,
    out_k: f32,
    /// samples of life left. A string is monophonic per slot and a new pluck resets this,
    /// so the only thing it buys is not running a string that has already rung out.
    ttl: u32,
    seed: u32,
}

impl Ks {
    pub fn new(rate: u32) -> Ks {
        let cap = ((rate as f32 / MIN_HZ).ceil() as usize + 4).next_power_of_two();
        Ks { buf: vec![0.0; cap], mask: cap - 1, idx: 0, rate: rate as f32,
             delay: 100.0, target: 100.0, glide: 0.0, damp: 0.3, lp: 0.0,
             ap_x1: 0.0, ap_y1: 0.0, loop_gain: 0.99, out_lp: 0.0, out_k: 0.5,
             ttl: 0, seed: 0x2545F491 }
    }

    fn rng(&mut self) -> f32 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 17;
        self.seed ^= self.seed << 5;
        (self.seed >> 8) as f32 / 8_388_608.0 - 1.0
    }

    /// Longest delay the line can hold, so a slide cannot walk off the end of the buffer.
    fn max_delay(&self) -> f32 { self.buf.len() as f32 - 4.0 }

    /// Strike the string. `bright` is 0..1 from velocity: a harder hit is a harder pick,
    /// which is brightness, not only level -- the level is applied downstream by the voice.
    pub fn pluck(&mut self, f0: f32, bright: f32, p: &Params, damp: f32) {
        let f0 = f0.clamp(MIN_HZ, self.rate / 4.0);
        self.delay = (self.rate / f0).min(self.max_delay());
        self.target = self.delay;
        self.glide = 0.0;
        self.damp = damp.clamp(0.0, 0.9);
        // Amplitude is multiplied by loop_gain once per trip round the loop and there are
        // f0 trips a second, so "-60 dB after decay_ms" is one exponential, solved per note.
        // Solving it per note rather than fixing the gain is what stops a high note dying in
        // a third of the time a low one takes.
        let passes = (f0 * p.decay_ms / 1000.0).max(1.0);
        self.loop_gain = (-(1000.0f32.ln()) / passes).exp().min(0.9999);
        self.out_k = (-std::f32::consts::TAU * p.lp_hz.max(200.0) / self.rate).exp();
        self.lp = 0.0;
        self.ap_x1 = 0.0;
        self.ap_y1 = 0.0;
        self.out_lp = 0.0;
        self.ttl = (self.rate * p.decay_ms / 1000.0 * 1.6) as u32;

        // ---- the excitation burst, written straight into the line. No scratch buffer:
        // this runs inside the audio callback and must not allocate. ---------------------
        let n = (self.delay as usize).max(2).min(self.buf.len());
        let nf = n as f32;
        // The string's shape at the moment of release: a triangle with its apex at the pick
        // point. This one line is the whole harmonic character -- see the module docs.
        let m = (p.pick.clamp(0.02, 0.5) * nf).clamp(1.0, nf - 1.0);
        // The plectrum's own noise, on top. Velocity opens up both how much and how bright,
        // so digging in is brighter as well as louder -- most of why a played bass sounds
        // played rather than triggered.
        let tone = (p.tone.clamp(0.0, 1.0) * (0.35 + 0.65 * bright.clamp(0.0, 1.0))).clamp(0.02, 1.0);
        let k = 1.0 - tone;
        let amt = 0.06 + 0.24 * tone;
        let mut z = 0.0;
        let mut sum = 0.0;
        for i in 0..n {
            let x = i as f32;
            let tri = if x <= m { x / m } else { (nf - x) / (nf - m) };
            let w = self.rng();
            z = (1.0 - k) * w + k * z;
            let v = tri * (1.0 - amt) + z * amt;
            self.buf[i] = v;
            sum += v;
        }
        // Take the mean out. A triangle is all one sign, and a delay line carrying a DC
        // offset does not ring -- it decays as a thump underneath the note.
        let mean = sum / nf;
        for s in &mut self.buf[..n] { *s -= mean; }
        // Normalise, so every note starts at the same level whatever the pick position and
        // the noise just did to it.
        let peak = self.buf[..n].iter().fold(0f32, |a, s| a.max(s.abs()));
        if peak > 1e-9 { let g = 0.5 / peak; for s in &mut self.buf[..n] { *s *= g; } }
        for s in &mut self.buf[n..] { *s = 0.0; }
        // The write cursor trails the read cursor by exactly the loop length, so starting it
        // at n puts the first read on the front of the burst.
        self.idx = n;
    }

    /// Slide to a new note without re-exciting: the delay line changes length under a string
    /// that is already ringing, which is what a finger moving up the neck does.
    pub fn glide_to(&mut self, f0: f32, ms: f32) {
        let f0 = f0.clamp(MIN_HZ, self.rate / 4.0);
        self.target = (self.rate / f0).min(self.max_delay());
        let n = (self.rate * ms.max(1.0) / 1000.0).max(1.0);
        self.glide = (self.target - self.delay) / n;
    }

    /// Raise the damping mid-note: the palm landing on the strings.
    pub fn set_damp(&mut self, damp: f32) { self.damp = damp.clamp(0.0, 0.9); }

    pub fn done(&self) -> bool { self.ttl == 0 }

    pub fn next(&mut self) -> f32 {
        if self.glide != 0.0 {
            if (self.target - self.delay).abs() <= self.glide.abs() {
                self.delay = self.target;
                self.glide = 0.0;
            } else {
                self.delay += self.glide;
            }
        }
        // Take the damping filter's own delay out of the line, then split what is left into
        // whole samples and a fraction for the allpass. The fraction is kept in 0.1..1.1
        // rather than 0..1 because a first-order allpass is ill-conditioned near zero.
        let l = (self.delay - self.damp / (1.0 - self.damp)).max(2.0);
        let n = (l - 0.1) as usize;
        let frac = (l - n as f32).clamp(0.1, 1.1);
        let a = (1.0 - frac) / (1.0 + frac);

        let out = self.buf[self.idx.wrapping_sub(n) & self.mask];
        self.lp = (1.0 - self.damp) * out + self.damp * self.lp;
        let x = self.lp * self.loop_gain;
        let y = a * x + self.ap_x1 - a * self.ap_y1;
        self.ap_x1 = x;
        self.ap_y1 = y;
        self.buf[self.idx & self.mask] = y;
        self.idx = self.idx.wrapping_add(1);
        self.ttl = self.ttl.saturating_sub(1);

        self.out_lp = (1.0 - self.out_k) * out + self.out_k * self.out_lp;
        self.out_lp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 44100;

    /// Period of `x` by autocorrelation, searched around `expect` samples, with a parabolic
    /// fit between bins -- without it the answer is quantised to whole samples, which is the
    /// very error being measured.
    fn period(x: &[f32], expect: f32) -> f32 {
        let lo = ((expect * 0.88) as usize).max(2);
        let hi = (expect * 1.12) as usize;
        let n = x.len() - hi - 1;
        let corr = |lag: usize| -> f32 { (0..n).map(|i| x[i] * x[i + lag]).sum() };
        let mut best = lo;
        let mut best_v = f32::MIN;
        for lag in lo..hi {
            let c = corr(lag);
            if c > best_v { best_v = c; best = lag; }
        }
        let (a, b, c) = (corr(best - 1), best_v, corr(best + 1));
        let d = a - 2.0 * b + c;
        best as f32 + if d.abs() > 1e-12 { 0.5 * (a - c) / d } else { 0.0 }
    }

    fn run(note: u8, secs: f32, p: &Params) -> Vec<f32> {
        let mut k = Ks::new(RATE);
        k.pluck(note_hz(note as f32), 1.0, p, p.damp);
        (0..(RATE as f32 * secs) as usize).map(|_| k.next()).collect()
    }

    fn cents(got: f32, want: f32) -> f32 { 1200.0 * (got / want).log2() }

    #[test]
    fn note_names_round_trip_through_the_parser() {
        // The roots of a song in D, which is the case this has to get right.
        for (name, midi) in [("D2", 38u8), ("B1", 35), ("A1", 33), ("G1", 31), ("E1", 28), ("F#1", 30)] {
            assert_eq!(parse_note(name), Some(midi), "{name}");
            assert_eq!(note_name(midi), name, "{midi}");
        }
        assert_eq!(parse_note("Gb1"), parse_note("F#1"), "flats and sharps are the same key");
        assert_eq!(parse_note("a1"), Some(33), "case does not matter");
        assert_eq!(parse_note("38"), Some(38), "a bare MIDI number passes through");
        assert_eq!(note_hz(parse_note("A2").unwrap() as f32), 110.0);
        assert_eq!(parse_note("H2"), None);
        assert_eq!(parse_note("D"), None, "an octave is not optional");
    }

    #[test]
    fn tuning_lands_within_ten_cents() {
        let p = Params::default();
        // The bottom octave and a half of a bass, where a whole-sample rounding error is
        // worth tens of cents and naive Karplus-Strong goes audibly flat.
        for note in [28u8, 30, 31, 33, 35, 38, 43, 50] {
            let want = note_hz(note as f32);
            let x = run(note, 0.6, &p);
            let got = RATE as f32 / period(&x[RATE as usize / 5..], RATE as f32 / want);
            assert!(cents(got, want).abs() < 10.0,
                    "note {note}: wanted {want:.2} Hz, got {got:.2} ({:+.1} cents)", cents(got, want));
        }
    }

    /// Energy in a narrow band around `f0 * h`, over a window of sustain.
    fn partials(x: &[f32], f0: f32, count: usize) -> Vec<f32> {
        let seg = &x[RATE as usize / 40..RATE as usize / 40 + RATE as usize * 3 / 10];
        // One Goertzel-ish sum per partial: cheaper than pulling in an FFT for eight bins.
        (1..=count).map(|h| {
            let w = std::f32::consts::TAU * f0 * h as f32 / RATE as f32;
            let (mut re, mut im) = (0.0f32, 0.0f32);
            for (i, s) in seg.iter().enumerate() {
                let t = w * i as f32;
                re += s * t.cos();
                im += s * t.sin();
            }
            (re * re + im * im) / (seg.len() * seg.len()) as f32
        }).collect()
    }

    /// The one that matters for a bass, and the one the old white-noise excitation failed:
    /// picked anywhere sane, the fundamental has to be the loudest partial. With a flat
    /// noise burst and a bridge-ish pick position it came out 9 dB under the fourth.
    #[test]
    fn the_fundamental_is_the_loudest_partial() {
        for (note, pick) in [(28u8, 0.12f32), (28, 0.25), (33, 0.12), (38, 0.12), (38, 0.4)] {
            let p = Params { pick, decay_ms: 2000.0, ..Default::default() };
            let f0 = note_hz(note as f32);
            let ps = partials(&run(note, 0.4, &p), f0, 6);
            let loudest = ps.iter().enumerate().max_by(|a, b| a.1.partial_cmp(b.1).unwrap()).unwrap().0;
            let db = |i: usize| 10.0 * (ps[i] / ps[0]).log10();
            assert_eq!(loudest, 0,
                "note {note} pick {pick}: partial {} is loudest; h2..h6 are {:?} dB under h1",
                loudest + 1, (1..6).map(|i| (db(i) * 10.0).round() / 10.0).collect::<Vec<_>>());
        }
    }

    /// Picking near the bridge cannot excite the harmonic with a node there: pick = 1/4
    /// must notch the fourth. That is the comb, and it is what makes a pick position audible
    /// at all rather than just a tone control.
    #[test]
    fn the_pick_position_notches_its_own_harmonic() {
        let f0 = note_hz(28.0);
        let ps = partials(&run(28, 0.4, &Params { pick: 0.25, decay_ms: 2000.0, ..Default::default() }), f0, 6);
        let db = 10.0 * (ps[3] / ps[2]).log10();
        assert!(db < -9.0, "pick=1/4 should notch h4; it is only {db:.1} dB under h3");
    }

    #[test]
    fn decay_reaches_minus_60db_near_the_stated_time() {
        let p = Params { decay_ms: 800.0, ..Default::default() };
        let x = run(28, 1.2, &p);
        let peak = |w: &[f32]| w.iter().fold(0f32, |a, s| a.max(s.abs()));
        let start = peak(&x[..RATE as usize / 20]);
        let at_decay = peak(&x[(RATE as f32 * 0.78) as usize..(RATE as f32 * 0.82) as usize]);
        let db = 20.0 * (at_decay / start).log10();
        assert!((-75.0..-45.0).contains(&db), "at 800 ms the string was {db:.1} dB down, wanted about -60");
    }

    #[test]
    fn damping_costs_the_string_its_highs_not_its_pitch() {
        let p = Params::default();
        let bright = run(33, 0.5, &Params { damp: 0.1, ..p.clone() });
        let dull = run(33, 0.5, &Params { damp: 0.85, ..p.clone() });
        // Mean absolute slew over RMS is a cheap high-frequency meter: a bright string
        // jitters between neighbouring samples, a dull one wanders.
        let slew = |x: &[f32]| x.windows(2).map(|w| (w[1] - w[0]).abs()).sum::<f32>() / x.len() as f32;
        let rms = |x: &[f32]| (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt();
        let w = RATE as usize / 10..RATE as usize / 4;
        let (b, d) = (slew(&bright[w.clone()]) / rms(&bright[w.clone()]),
                      slew(&dull[w.clone()]) / rms(&dull[w.clone()]));
        assert!(b > d * 1.5, "damping did not take the highs off: bright {b:.4} vs dull {d:.4}");
        let want = note_hz(33.0);
        let got = RATE as f32 / period(&dull[RATE as usize / 8..], RATE as f32 / want);
        assert!(cents(got, want).abs() < 15.0, "damping pulled the pitch: {got:.2} vs {want:.2}");
    }

    #[test]
    fn a_slide_arrives_at_the_target_note() {
        let p = Params { decay_ms: 3000.0, ..Default::default() };
        let mut k = Ks::new(RATE);
        k.pluck(note_hz(28.0), 1.0, &p, p.damp);
        for _ in 0..RATE as usize / 20 { k.next(); }
        k.glide_to(note_hz(33.0), 60.0);
        let x: Vec<f32> = (0..RATE as usize / 2).map(|_| k.next()).collect();
        let want = note_hz(33.0);
        let got = RATE as f32 / period(&x[RATE as usize / 8..], RATE as f32 / want);
        assert!(cents(got, want).abs() < 15.0, "slide landed on {got:.2} Hz, wanted {want:.2}");
    }
}
