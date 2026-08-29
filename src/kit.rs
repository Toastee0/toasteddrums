//! A kit = up to 9 voices (one per annunciator tile, 3×3) each backed by one toykeyboards hit.
//! Kit file format (plain lines, `#` comments):
//!   name <kit name>
//!   voice <slot 0-8> <label> <r,g,b> <path/to/hit.wav>
//! Paths are relative to the kit file's directory.

use crate::wav::Wav;
use std::path::Path;

#[derive(Clone, Debug)]
pub struct Voice {
    pub label: String,
    pub color: [u8; 3],
    pub mono: Vec<f32>,
}

#[derive(Debug)]
pub struct Kit {
    pub name: String,
    pub rate: u32,
    pub voices: [Option<Voice>; 9],
}

impl Kit {
    pub fn load(path: &Path) -> Result<Kit, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let dir = path.parent().unwrap_or(Path::new("."));
        let mut kit = Kit { name: "untitled".into(), rate: 0, voices: Default::default() };
        for (ln, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            let mut it = line.splitn(2, ' ');
            let key = it.next().unwrap();
            let rest = it.next().unwrap_or("").trim();
            match key {
                "name" => kit.name = rest.to_string(),
                "voice" => {
                    // slot label colour path — path may contain spaces, so peel 3 tokens
                    let mut s = rest;
                    let mut tok = || -> Option<&str> {
                        s = s.trim_start();
                        let end = s.find(char::is_whitespace).unwrap_or(s.len());
                        let (t, r) = s.split_at(end);
                        s = r;
                        (!t.is_empty()).then_some(t)
                    };
                    let slot: usize = tok().and_then(|s| s.parse().ok()).filter(|&s| s < 9)
                        .ok_or(format!("line {}: slot must be 0-8", ln + 1))?;
                    let label = tok().ok_or("missing label")?.to_string();
                    let color = parse_rgb(tok().ok_or("missing colour")?)?;
                    let file = s.trim();
                    if file.is_empty() { return Err(format!("line {}: missing path", ln + 1)); }
                    let bytes = std::fs::read(dir.join(file)).map_err(|e| format!("{file}: {e}"))?;
                    let w = Wav::parse(&bytes).map_err(|e| format!("{file}: {e}"))?;
                    if kit.rate == 0 { kit.rate = w.rate; }
                    else if w.rate != kit.rate { return Err(format!("{file}: rate {} != kit rate {}", w.rate, kit.rate)); }
                    kit.voices[slot] = Some(Voice { label, color, mono: w.mono() });
                }
                _ => return Err(format!("line {}: unknown key {key}", ln + 1)),
            }
        }
        if kit.rate == 0 { return Err("kit has no voices".into()); }
        Ok(kit)
    }
}

fn parse_rgb(s: &str) -> Result<[u8; 3], String> {
    let v: Vec<u8> = s.split(',').map(|x| x.trim().parse().map_err(|_| format!("bad colour {s}"))).collect::<Result<_, _>>()?;
    if v.len() != 3 { return Err(format!("bad colour {s}")); }
    Ok([v[0], v[1], v[2]])
}
