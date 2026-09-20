//! ToastedDrums — toy-keyboard drum machine. Bench commands run anywhere; the live command
//! (audio out + annunciator serial) is the laptop build, next step.
//!
//!   toasteddrums render <kit> <pattern> <out.wav> [bars]   offline mix → WAV (bench)
//!   toasteddrums show   <kit> <pattern>                    print each step's 3×3 frame
//!   toasteddrums bake   <kit> <song.json> <outdir>      export for a game: slot WAVs + song.txt + golden.wav
//!   toasteddrums pads   [port] [baud]                      watch the pad controller
//!   toasteddrums live   <kit> [port] [map]                 PLAY the kit from the pads

mod audio;
mod kit;
mod live;
mod mcp;
mod midi;
mod pads;
mod seq;
mod song;
mod string;
mod ui;
mod vis;
mod wav;
mod wub;

use std::path::Path;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let r = match a.get(1).map(String::as_str) {
        Some("render") if a.len() >= 5 => render(&a[2], &a[3], &a[4], a.get(5).and_then(|b| b.parse().ok()).unwrap_or(2)),
        Some("show") if a.len() >= 4 => show(&a[2], &a[3]),
        Some("bake") if a.len() >= 5 => bake(&a[2], &a[3], &a[4]),
        // `song <kit> <song.json> <out.wav> [steps] [master]`: the tracker's own model,
        // rather than the bench's flat .pat, rendered straight to a WAV.
        Some("song") if a.len() >= 5 => render_song(&a[2], &a[3], &a[4],
            a.get(5).and_then(|n| n.parse().ok()),
            a.get(6).and_then(|m| m.parse().ok()).unwrap_or(song::DEFAULT_MASTER)),
        // The tracker as an MCP server over stdio: `mcp [kit] [COMn] [door-port]`, the last
        // two in either order. Nothing but protocol may go to stdout in this mode. This arm
        // yields a Result like every other -- `?` cannot be used in `fn main`, which returns ().
        Some("mcp") => {
            let (kit, pads, door) = engine_args(&a[2..]);
            find_data(kit).and_then(|p| mcp::serve(&p.display().to_string(), pads, door))
        }
        // The operator UI: `ui [door-port] [kit] [COMn]`. Attaches to a running `mcp`'s
        // door; if nothing is listening it runs the engine itself, so the machine works
        // with no Claude at all. Either way there is one engine and one song.
        Some("ui") => {
            let (kit, pads, door) = engine_args(&a[2..]);
            find_data(kit).and_then(|p| ui::run(door.unwrap_or(ui::DEFAULT_DOOR), &p.display().to_string(), pads))
        }
        // Diagnostic: list MIDI inputs, then open one and print what it sends.
        Some("midi") => midi_diag(a.get(2).and_then(|s| s.parse().ok())),
        // Diagnostic: push a known-good file straight through the output layer, bypassing
        // the pads and the live mixer. If THIS crackles, audio.rs is at fault; if it is
        // clean, the fault is upstream in how live mixes.
        Some("play") if a.len() >= 3 => play_wav(&a[2]),
        // Purer still: a synthesised sine, no file involved at all. The cleanest possible
        // test of the output layer. Optional Hz, default 110.
        Some("tone") => play_tone(a.get(2).and_then(|s| s.parse().ok()).unwrap_or(110.0)),
        Some("pads") => watch_pads(
            a.get(2).map(String::as_str).unwrap_or("COM5"),
            a.get(3).and_then(|b| b.parse().ok()).unwrap_or(115200),
        ),
        // `live` with no kit is the common case, so default it. A first argument that
        // looks like a port is treated as one, so `live COM7` does the obvious thing.
        Some("live") => {
            let looks_like_port = |s: &String| s.len() >= 4 && s[..3].eq_ignore_ascii_case("com");
            let (kit_arg, rest) = match a.get(2) {
                Some(s) if looks_like_port(s) => ("kits/mt240.kit", 2),
                Some(s) => (s.as_str(), 3),
                None => ("kits/mt240.kit", 3),
            };
            go_live(kit_arg,
                    a.get(rest).map(String::as_str).unwrap_or("COM5"),
                    a.get(rest + 1).map(String::as_str),
                    a.get(rest + 2).and_then(|g| g.parse().ok()).unwrap_or(live::DEFAULT_MASTER))
        }
        _ => Err(concat!(
            "usage: toasteddrums render <kit> <pattern> <out.wav> [bars]\n",
            "                  show   <kit> <pattern>\n",
            "                  song   <kit> <song.json> <out.wav> [steps] [master]  tracker model → WAV\n",
            "                  pads   [port] [baud]\n",
            "                  live   <kit> [port] [map] [gain]   map e.g. 0,3,1,8 = pad→slot; gain default 2.0\n",
            "                  mcp    [kit] [COMn] [door-port]     MCP server on stdio (kit default kits/bigbeat.kit)\n",
            "                  ui     [door-port] [kit] [COMn]     tracker UI; attaches to mcp's door or runs the engine itself\n",
            "                  play   <wav> | tone [hz]          output-layer diagnostics\n",
            "                  midi   [id]                        list MIDI inputs; open one and print notes",
        ).into()),
    };
    if let Err(e) = r { eprintln!("toasteddrums: {e}"); std::process::exit(1); }
}

/// Engine arguments by shape rather than position: `COMn` is the pads port, a number is
/// the door port, anything else is the kit. So `mcp COM5 4242`, `ui 4242`, `ui kits/x.kit
/// COM5` all read the obvious way.
fn engine_args(rest: &[String]) -> (&str, Option<&str>, Option<u16>) {
    let mut kit = "kits/bigbeat.kit";
    let (mut pads, mut door) = (None, None);
    for s in rest {
        if s.len() >= 4 && s[..3].eq_ignore_ascii_case("com") { pads = Some(s.as_str()); }
        else if let Ok(p) = s.parse::<u16>() { door = Some(p); }
        else { kit = s.as_str(); }
    }
    (kit, pads, door)
}

fn go_live(kit_path: &str, port: &str, map_arg: Option<&str>, master: f32) -> Result<(), String> {
    let k = kit::Kit::load(&find_data(kit_path)?)?;
    let map: Vec<usize> = match map_arg {
        Some(s) => {
            let v: Result<Vec<usize>, _> = s.split(',').map(|t| t.trim().parse::<usize>()).collect();
            let v = v.map_err(|_| format!("bad map '{s}': want comma-separated slot numbers"))?;
            if let Some(bad) = v.iter().find(|&&i| i > 8) {
                return Err(format!("bad map: slot {bad} is out of range 0-8"));
            }
            v
        }
        None => live::DEFAULT_MAP.to_vec(),
    };
    live::run(k, port, 115200, map, master)
}

/// Lists MIDI inputs; with an id (default 0 when any exist) opens it and prints every note
/// for 30 s. The manual check for the Casio: play a key, see `on ch0 note 60 vel 87`.
fn midi_diag(id: Option<u32>) -> Result<(), String> {
    let devs = midi::Midi::list();
    if devs.is_empty() { return Err("no MIDI input devices".into()); }
    for (i, n) in &devs { eprintln!("{i}: {n}"); }
    let id = id.unwrap_or(devs[0].0);
    let mut m = midi::Midi::open(id)?;
    eprintln!("opened {id} ({}) — play something; 30 s", m.name);
    let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < end {
        for e in m.poll() {
            println!("{} ch{} note {:>3} vel {:>3}", if e.on { "on " } else { "off" }, e.channel, e.note, e.vel);
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    Ok(())
}

/// Plays a WAV through audio.rs and nothing else. No pads, no mixer, no threads: if this
/// is clean the output layer is sound and the fault is upstream; if it crackles, audio.rs
/// itself is broken. `render`'s output is the natural input, since seq.rs is known good.
fn play_wav(path: &str) -> Result<(), String> {
    let p = find_data(path)?;
    let bytes = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    let w = wav::Wav::parse(&bytes)?;
    let mono = w.mono();
    let peak = mono.iter().fold(0f32, |m, s| m.max(s.abs()));
    eprintln!("{}: {} Hz, {} frames ({:.2} s), peak {peak:.3}",
              p.display(), w.rate, mono.len(), mono.len() as f32 / w.rate as f32);

    let secs = mono.len() as f32 / w.rate as f32;
    // Device-native rate; pitch-correct the file onto it with a fractional cursor, the
    // same way live.rs does. Asking the device for the file's rate is the bug we just fixed.
    let dev = audio::device_rate()?;
    let ratio = w.rate as f32 / dev as f32;
    let last = mono.len().saturating_sub(1);
    let mut pos = 0f32;
    let t0 = std::time::Instant::now();
    let out = audio::Out::open(move |buf| {
        for o in buf.iter_mut() {
            let i = pos as usize;
            if i >= last { break; }
            let frac = pos - i as f32;
            *o = mono[i] * (1.0 - frac) + mono[i + 1] * frac;
            pos += ratio;
        }
    })?;
    std::thread::sleep(std::time::Duration::from_secs_f32(secs + 0.2));
    report_rate(&out, t0);
    Ok(())
}

/// The direct test for "playing slower than intended": frames the device consumed divided
/// by wall time should equal the sample rate. Gaps or stalls show up as a low number.
fn report_rate(out: &audio::Out, t0: std::time::Instant) {
    let elapsed = t0.elapsed().as_secs_f32();
    let frames = out.frames_delivered();
    let effective = frames as f32 / elapsed;
    eprintln!("done. {frames} frames in {elapsed:.2} s = {effective:.0} Hz effective (device {}; \
               {:.1}% of target)  ~{:.1} ms per callback",
              out.rate, effective / out.rate as f32 * 100.0, out.latency_ms());
}

/// Two seconds of sine at `hz`, generated on the fly, through audio.rs. Nothing else in the
/// chain — no WAV, no mixer, no pads. A sine is the one signal where any crackle, buzz or
/// stutter is unambiguously the output layer's doing.
fn play_tone(hz: f32) -> Result<(), String> {
    // Generated directly at the device's rate, so the pitch is exact by construction.
    let rate = audio::device_rate()?;
    eprintln!("sine {hz} Hz, 2 s, amplitude 0.5, at device rate {rate} Hz");
    let mut n = 0usize;
    let step = 2.0 * std::f32::consts::PI * hz / rate as f32;
    let t0 = std::time::Instant::now();
    let out = audio::Out::open(move |buf| {
        for s in buf.iter_mut() {
            *s = 0.5 * (n as f32 * step).sin();
            n += 1;
        }
    })?;
    std::thread::sleep(std::time::Duration::from_millis(2000));
    report_rate(&out, t0);
    Ok(())
}

/// Opens the controller, does the PROTOCOL.md handshake, and prints hits until Ctrl-C.
/// This is the link the sequencer will consume; for now it just proves the wire.
fn watch_pads(port: &str, baud: u32) -> Result<(), String> {
    let mut p = pads::Pads::open(port, baud)?;
    eprintln!("{port} @ {baud} — handshaking");
    p.start("toasteddrums")?;

    loop {
        for m in p.poll()? {
            match m {
                pads::Msg::Hit(h) => {
                    let bar = "#".repeat((h.vel as usize / 4).max(1));
                    // depth above the pad's gain means vel clipped at 127 — flag it, since
                    // a clipped kit plays flat and that is the usual first thing to fix.
                    let clip = if h.vel >= 127 { " CLIP" } else { "" };
                    println!("{:<4} vel {:>3} {bar:<32} depth {:>5.1}%  slope {:>5.2}{clip}",
                             h.name, h.vel, h.depth, h.slope);
                }
                pads::Msg::Pad { pad, name, thresh, slope, gain, base } =>
                    eprintln!("  pad {pad} {name:<4} thresh={thresh} slope={slope} gain={gain} base={base}"),
                pads::Msg::Reply(r) if !r.ok =>
                    eprintln!("  err {} {}", r.code.unwrap_or_default(), r.text),
                pads::Msg::Reply(r) => eprintln!("  ok {}", r.text),
                pads::Msg::Config(c) => eprintln!("  {c}"),
                pads::Msg::Notice(n) => eprintln!("  ! {n}"),
                pads::Msg::Data(_) => {}
                pads::Msg::Unknown(u) => eprintln!("  ? {u}"),
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

/// Finds a data file without caring what the working directory is.
///
/// Running `target\release\toasteddrums.exe live kits/mt240.kit` from inside
/// `target\release` used to fail, because the path is relative to the repo root. Requiring
/// the user to cd first is a poor trade for a program you are supposed to pick up and play,
/// so: try it as given, then walk up from the executable, then up from the cwd. Sample
/// paths inside a kit stay relative to the kit file itself, so finding the kit is enough.
fn find_data(rel: &str) -> Result<std::path::PathBuf, String> {
    let p = Path::new(rel);
    if p.exists() { return Ok(p.to_path_buf()); }
    if p.is_absolute() { return Err(format!("{rel}: not found")); }

    let mut tried = Vec::new();
    let mut roots: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        let mut d = exe.parent().map(|d| d.to_path_buf());
        while let Some(dir) = d {
            roots.push(dir.clone());
            d = dir.parent().map(|d| d.to_path_buf());
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        let mut d = Some(cwd);
        while let Some(dir) = d {
            roots.push(dir.clone());
            d = dir.parent().map(|d| d.to_path_buf());
        }
    }
    for root in roots {
        let cand = root.join(rel);
        if cand.exists() { return Ok(cand); }
        if tried.len() < 4 { tried.push(cand.display().to_string()); }
    }
    Err(format!("{rel}: not found. Looked in {} and parents of the exe and cwd",
                tried.join(", ")))
}

fn load(kit: &str, pat: &str) -> Result<(kit::Kit, seq::Pattern), String> {
    let k = kit::Kit::load(&find_data(kit)?)?;
    let pp = find_data(pat)?;
    let p = seq::Pattern::parse(&std::fs::read_to_string(&pp)
        .map_err(|e| format!("{}: {e}", pp.display()))?)?;
    Ok((k, p))
}

fn render(kit: &str, pat: &str, out: &str, bars: usize) -> Result<(), String> {
    let (k, p) = load(kit, pat)?;
    let mut e = seq::Engine::new(&k, p);
    let data = e.render(bars);
    let peak = data.iter().fold(0f32, |m, s| m.max(s.abs()));
    let w = wav::Wav { rate: k.rate, channels: 1, data };
    std::fs::write(out, w.to_bytes()).map_err(|e| e.to_string())?;
    eprintln!("kit '{}' @ {} Hz, {} bpm, {bars} bars → {out} ({:.2} s, peak {peak:.2})",
        k.name, k.rate, e.pattern.bpm, w.frames() as f32 / k.rate as f32);
    Ok(())
}

fn render_song(kit: &str, song_path: &str, out: &str, steps: Option<usize>, master: f32) -> Result<(), String> {
    let k = std::sync::Arc::new(kit::Kit::load(&find_data(kit)?)?);
    let sp = find_data(song_path)?;
    let mut s: song::Song = serde_json::from_str(&std::fs::read_to_string(&sp)
        .map_err(|e| format!("{}: {e}", sp.display()))?)
        .map_err(|e| format!("{}: {e}", sp.display()))?;
    s.normalise();
    let steps = steps.unwrap_or_else(|| s.cycle_steps());
    let (bpm, tracks) = (s.bpm, s.tracks.len());
    let data = song::Transport::render(k.clone(), s, steps, master);
    let peak = data.iter().fold(0f32, |m, x| m.max(x.abs()));
    let rms = (data.iter().map(|x| x * x).sum::<f32>() / data.len() as f32).sqrt();
    let w = wav::Wav { rate: k.rate, channels: 1, data };
    std::fs::write(out, w.to_bytes()).map_err(|e| format!("{out}: {e}"))?;
    eprintln!("kit '{}' @ {} Hz, {bpm} bpm, {tracks} tracks, {steps} steps, master {master} → {out} \
               ({:.2} s, peak {peak:.2}, rms {rms:.3}, crest {:.1} dB)",
              k.name, k.rate, w.frames() as f32 / k.rate as f32, 20.0 * (peak / rms.max(1e-9)).log10());
    Ok(())
}

/// `bake <kit> <song.json> <outdir>`: everything a game needs to play the song with no kit
/// loader, resampler or JSON parser of its own. Written to `outdir`:
///   slotN.wav   each sample voice after its load-time mutations, 32-bit float mono at
///               the kit rate (float so the game's mix can match ours bit for bit)
///   song.txt    bpm, rate, master, voices (wubs as parameters, samples as file names),
///               tracks with intensity, one `hit` line per hit with its mods
///   song.json   the song as given, for loading back into the tracker
///   golden.wav  one full polymeter cycle plus a second of tail, rendered by the
///               Transport: the game's port is diffed against this
fn bake(kit_path: &str, song_path: &str, outdir: &str) -> Result<(), String> {
    use std::fmt::Write as _;
    let kp = find_data(kit_path)?;
    let k = std::sync::Arc::new(kit::Kit::load(&kp)?);
    let sp = find_data(song_path)?;
    let text = std::fs::read_to_string(&sp).map_err(|e| format!("{}: {e}", sp.display()))?;
    let mut song: song::Song = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", sp.display()))?;
    song.normalise();
    let out = Path::new(outdir);
    std::fs::create_dir_all(out).map_err(|e| format!("{outdir}: {e}"))?;

    let mut s = String::new();
    let _ = writeln!(s, "# baked by toasteddrums from {} + {}", kp.display(), sp.display());
    let _ = writeln!(s, "bpm {}", song.bpm);
    let _ = writeln!(s, "rate {}", k.rate);
    let _ = writeln!(s, "master {}", song::DEFAULT_MASTER);
    for (i, v) in k.voices.iter().enumerate() {
        let Some(v) = v else { continue };
        let name = v.label.replace(' ', "_");
        if let Some(p) = &v.wub {
            let _ = write!(s, "voice slot={i} name={name} wub f0={} wave={} detune={} sub={} cutoff={} floor={} res={} wob={} hold={} decay={} gain={}",
                           p.f0, p.wave, p.detune_cents, p.sub, p.cutoff, p.floor, p.res, p.wob, p.hold_ms, p.decay_ms, p.gain);
            if let Some(d) = p.drive { let _ = write!(s, " drive={d}"); }
            if let Some(c) = p.crush { let _ = write!(s, " crush={c}"); }
            let _ = writeln!(s);
        } else {
            let file = format!("slot{i}.wav");
            let w = wav::Wav { rate: k.rate, channels: 1, data: v.mono.clone() };
            std::fs::write(out.join(&file), w.to_bytes_f32()).map_err(|e| format!("{file}: {e}"))?;
            let _ = writeln!(s, "voice slot={i} name={name} file={file}");
        }
    }
    let mut nhits = 0;
    for t in &song.tracks {
        let _ = writeln!(s, "track name={} len={} intensity={}{}", t.name.replace(' ', "_"), t.len, t.intensity, if t.mute { " mute" } else { "" });
        for (step, cell) in t.cells.iter().enumerate() {
            for h in cell {
                let _ = write!(s, "hit step={step} slot={} vel={}", h.slot, h.vel);
                let m = &h.mods;
                if let Some(v) = m.pitch { let _ = write!(s, " pitch={v}"); }
                if let Some(v) = m.drive { let _ = write!(s, " drive={v}"); }
                if let Some(v) = m.crush { let _ = write!(s, " crush={v}"); }
                if m.rev == Some(true) { let _ = write!(s, " rev"); }
                if let Some(v) = m.gain { let _ = write!(s, " gain={v}"); }
                if let Some(v) = m.decay_ms { let _ = write!(s, " decay={v}"); }
                let _ = writeln!(s);
                nhits += 1;
            }
        }
    }
    std::fs::write(out.join("song.txt"), &s).map_err(|e| format!("song.txt: {e}"))?;
    std::fs::write(out.join("song.json"), serde_json::to_string_pretty(&song).unwrap()).map_err(|e| format!("song.json: {e}"))?;

    let steps = song.cycle_steps();
    let data = song::Transport::render(k.clone(), song.clone(), steps, song::DEFAULT_MASTER);
    let peak = data.iter().fold(0f32, |m, s| m.max(s.abs()));
    let w = wav::Wav { rate: k.rate, channels: 1, data };
    std::fs::write(out.join("golden.wav"), w.to_bytes_f32()).map_err(|e| format!("golden.wav: {e}"))?;
    eprintln!("baked '{}' + {} tracks / {nhits} hits @ {} Hz {} bpm → {outdir} (golden {:.2} s, peak {peak:.2})",
              k.name, song.tracks.len(), k.rate, song.bpm, w.frames() as f32 / k.rate as f32);
    Ok(())
}

fn show(kit: &str, pat: &str) -> Result<(), String> {
    let (k, p) = load(kit, pat)?;
    let sps = p.samples_per_step(k.rate);
    let mut e = seq::Engine::new(&k, p);
    let mut chunk = vec![0.0f32; sps];
    for v in 0..9 { if let Some(vo) = &k.voices[v] { eprintln!("slot {v} ({},{}) {}", v % 3, v / 3, vo.label); } }
    for _ in 0..16 {
        let s = e.next_step();
        let f = vis::frame(&k, &e.glow, s);
        let mut line = format!("step {s:2}: ");
        for t in 0..9 {
            let (r, g, b) = (f[t * 3], f[t * 3 + 1], f[t * 3 + 2]);
            let lum = r.max(g).max(b);
            line.push(match lum { 0..=15 => '.', 16..=80 => 'o', _ => '#' });
            if t % 3 == 2 { line.push(' '); }
        }
        println!("{line} pkt {} B", vis::paint_packet(1, &f).len());
        e.mix(&mut chunk);
    }
    Ok(())
}
