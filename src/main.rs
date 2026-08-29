//! ToastedDrums — toy-keyboard drum machine. Bench commands run anywhere; the live command
//! (audio out + annunciator serial) is the laptop build, next step.
//!
//!   toasteddrums render <kit> <pattern> <out.wav> [bars]   offline mix → WAV (bench)
//!   toasteddrums show   <kit> <pattern>                    print each step's 3×3 frame

mod kit;
mod seq;
mod vis;
mod wav;

use std::path::Path;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let r = match a.get(1).map(String::as_str) {
        Some("render") if a.len() >= 5 => render(&a[2], &a[3], &a[4], a.get(5).and_then(|b| b.parse().ok()).unwrap_or(2)),
        Some("show") if a.len() >= 4 => show(&a[2], &a[3]),
        _ => Err("usage: toasteddrums render <kit> <pattern> <out.wav> [bars] | show <kit> <pattern>".into()),
    };
    if let Err(e) = r { eprintln!("toasteddrums: {e}"); std::process::exit(1); }
}

fn load(kit: &str, pat: &str) -> Result<(kit::Kit, seq::Pattern), String> {
    let k = kit::Kit::load(Path::new(kit))?;
    let p = seq::Pattern::parse(&std::fs::read_to_string(pat).map_err(|e| format!("{pat}: {e}"))?)?;
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
