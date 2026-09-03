//! ToastedDrums — toy-keyboard drum machine. Bench commands run anywhere; the live command
//! (audio out + annunciator serial) is the laptop build, next step.
//!
//!   toasteddrums render <kit> <pattern> <out.wav> [bars]   offline mix → WAV (bench)
//!   toasteddrums show   <kit> <pattern>                    print each step's 3×3 frame
//!   toasteddrums pads   [port] [baud]                      watch the pad controller
//!   toasteddrums live   <kit> [port] [map]                 PLAY the kit from the pads

mod audio;
mod kit;
mod live;
mod pads;
mod seq;
mod vis;
mod wav;

use std::path::Path;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let r = match a.get(1).map(String::as_str) {
        Some("render") if a.len() >= 5 => render(&a[2], &a[3], &a[4], a.get(5).and_then(|b| b.parse().ok()).unwrap_or(2)),
        Some("show") if a.len() >= 4 => show(&a[2], &a[3]),
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
                    a.get(rest + 1).map(String::as_str))
        }
        _ => Err(concat!(
            "usage: toasteddrums render <kit> <pattern> <out.wav> [bars]\n",
            "                  show   <kit> <pattern>\n",
            "                  pads   [port] [baud]\n",
            "                  live   <kit> [port] [map]      map e.g. 0,3,1,8 = pad→slot",
        ).into()),
    };
    if let Err(e) = r { eprintln!("toasteddrums: {e}"); std::process::exit(1); }
}

fn go_live(kit_path: &str, port: &str, map_arg: Option<&str>) -> Result<(), String> {
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
    live::run(k, port, 115200, map)
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
