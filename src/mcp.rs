//! mcp — the engine as an MCP server, so Claude can play the tracker alongside the operator.
//!
//! One long-running process owns the audio device, the pads port and the song. This file is
//! the stdio door: newline-delimited JSON-RPC 2.0 per the Model Context Protocol. The UI
//! door speaks the same verbs over a localhost TCP listener (`serve_tcp`), through the same
//! `dispatch`, so both edit ONE song, never two copies.
//!
//! RULE: nothing but protocol goes to stdout. Diagnostics use stderr. `live`'s screen
//! drawing never runs in this mode. Any future `println!` in shared code breaks the MCP
//! handshake silently.
//!
//! THREADING. `Studio` is shared as `Arc<Studio>` across the stdio loop, TCP connection
//! threads, the pads thread and (later) the MIDI thread, so everything in it is Sync: the
//! canonical Song behind a Mutex, the audio command channel behind a Mutex (an
//! `mpsc::Sender` is `!Sync` on its own), the kit behind an RwLock, and the transport's
//! position as atomics that the audio callback writes directly. The audio thread holds its
//! own copy of the song and receives `Cmd`s; hits from pads/MIDI go through `record_hit`.

use crate::audio::{device_rate, Out};
use crate::kit::Kit;
use crate::midi::Midi;
use crate::pads::{Learn, Msg, Pads};
use crate::song::{quantise, Cmd, Hit, Mods, Resampler, Song, Track, Transport};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex, RwLock};

/// How the keyboard plays. Drums: notes map to kit slots through the context track's key
/// map. Chromatic: every note plays ONE slot, pitched by `2^((note-root)/12)` — the existing
/// per-hit pitch mod, so any sample or the synth kick becomes a bass or a lead.
#[derive(Clone, Debug)]
enum KeysMode {
    Drums,
    Chromatic { slot: usize, root: u8 },
}

/// The keyboard's own context, independent of the pads': its track, its mode, and (in the
/// studio) its own armed mods — so bass on the keys and drums on the feet record into
/// different tracks in one take.
#[derive(Clone, Debug)]
struct MidiState {
    id: u32,
    name: String,
    mode: KeysMode,
    track: usize,
}

struct PadsState {
    tx: Sender<String>,          // commands to the pads thread ("learn", or raw serial)
    names: Vec<String>,
    learned: Vec<Option<(f32, f32)>>,   // (thresh, gain) per pad once a learn completes
}

struct Studio {
    kit: RwLock<Arc<Kit>>,
    kit_path: Mutex<String>,
    song: Mutex<Song>,
    to_audio: Mutex<Sender<Cmd>>,
    /// Written by the audio callback every buffer; read by whoever quantises.
    playing: Arc<AtomicBool>,
    step: Arc<AtomicU64>,
    /// step phase * 1000
    phase_milli: Arc<AtomicU32>,
    context: AtomicU32,          // current track index for the pads
    recording: AtomicBool,
    armed: Mutex<Mods>,          // "any modifier, any note": rides on every recorded pad hit
    pads: Mutex<Option<PadsState>>,
    midi: Mutex<Option<MidiState>>,
    keys_armed: Mutex<Mods>,     // armed mods for hits recorded from the keyboard
    _out: Out,
}

impl Studio {
    fn send(&self, c: Cmd) -> Result<(), String> {
        self.to_audio.lock().unwrap().send(c).map_err(|_| "audio thread is gone".to_string())
    }
    fn push_song(&self) -> Result<(), String> {
        let s = self.song.lock().unwrap().clone();
        self.send(Cmd::SetSong(s))
    }
}

/// A live hit from any input: always sounds immediately; while recording and playing it is
/// also quantised into `track` at the nearest step, replacing any hit already there on the
/// same slot (a retap corrects rather than stacks). One lock covers reading the length,
/// quantising and inserting, so the track cannot shrink between the two.
fn record_hit(st: &Arc<Studio>, track: usize, hit: Hit) {
    let _ = st.send(Cmd::Trigger(hit.clone()));
    if !(st.recording.load(Ordering::Relaxed) && st.playing.load(Ordering::Relaxed)) { return; }
    let gs = st.step.load(Ordering::Relaxed);
    let phase = st.phase_milli.load(Ordering::Relaxed) as f32 / 1000.0;
    let changed = {
        let mut s = st.song.lock().unwrap();
        match s.tracks.get_mut(track) {
            Some(t) => {
                t.normalise();
                let cell = quantise(gs, phase, t.len);
                t.cells[cell].retain(|x| x.slot != hit.slot);
                t.cells[cell].push(hit);
                true
            }
            None => false,
        }
    };
    if changed { let _ = st.push_song(); }
}

// ---- audio thread -----------------------------------------------------------------------
fn start_audio(kit: Arc<Kit>, song: Song, playing: Arc<AtomicBool>, step: Arc<AtomicU64>,
               phase: Arc<AtomicU32>) -> Result<(Out, Sender<Cmd>), String> {
    let dev = device_rate()?;
    let (tx, rx) = channel::<Cmd>();
    let mut t = Transport::new(kit.clone(), song);
    let mut rs = Resampler::new(kit.rate, dev);
    let out = Out::open(move |buf| {
        while let Ok(c) = rx.try_recv() { t.apply(c); }
        rs.run(buf, |kb| t.fill(kb));
        playing.store(t.playing, Ordering::Relaxed);
        step.store(t.global_step, Ordering::Relaxed);
        phase.store((t.step_phase() * 1000.0) as u32, Ordering::Relaxed);
    })?;
    Ok((out, tx))
}

// ---- pads thread ------------------------------------------------------------------------
fn start_pads(st: Arc<Studio>, port: String) -> Result<(), String> {
    let mut pads = Pads::open(&port, 115200)?;
    pads.session_start("toasteddrums-mcp")?;
    let cal = pads.calibrate(std::time::Duration::from_millis(2500))?;
    if cal.is_empty() { return Err("no calibration lines from the device".into()); }
    pads.set_learn_thresholds(&cal)?;

    let (tx, rx) = channel::<String>();
    let names: Vec<String> = cal.iter().map(|c| c.name.clone()).collect();
    *st.pads.lock().unwrap() = Some(PadsState { tx, names, learned: vec![None; cal.len()] });

    std::thread::spawn(move || {
        let mut learn: Option<Learn> = None;
        loop {
            while let Ok(cmd) = rx.try_recv() {
                match cmd.as_str() {
                    "learn" => {
                        if pads.set_learn_thresholds(&cal).is_err() { break; }
                        learn = Some(Learn::new(&cal, std::time::Duration::from_secs(90)));
                        eprintln!("learn: tap each pad 3 times");
                    }
                    other => { let _ = pads.send(other); }
                }
            }
            let msgs = match pads.poll() { Ok(m) => m, Err(_) => break };
            for m in msgs {
                let h = match m { Msg::Hit(h) => h, _ => continue };
                if let Some(l) = learn.as_mut() {
                    if let Some((_, n)) = l.feed(&h) { eprintln!("learn: {} {n}/3 drop {:.0}", h.name, h.depth); }
                    continue;   // taps during a learn are calibration, not notes
                }
                let ctx = st.context.load(Ordering::Relaxed) as usize;
                let armed = st.armed.lock().unwrap().clone();
                let slot = {
                    let s = st.song.lock().unwrap();
                    s.tracks.get(ctx).map(|t| t.pads.get(h.pad as usize).copied().unwrap_or(0)).unwrap_or(0)
                };
                record_hit(&st, ctx, Hit { slot, vel: h.vel, mods: armed });
            }
            // Finish a learn: serial writes happen with NO Studio lock held, then a brief
            // lock stores the results.
            if learn.as_ref().map_or(false, |l| l.done()) {
                let l = learn.take().unwrap();
                let results = l.results();
                for (i, r) in results.iter().enumerate() {
                    if let Some((th, g)) = r { let _ = pads.apply_learned(i, *th, *g); }
                }
                if let Some(p) = st.pads.lock().unwrap().as_mut() { p.learned = results; }
                eprintln!("learn: done");
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        eprintln!("pads thread stopped");
    });
    Ok(())
}

// ---- midi thread ------------------------------------------------------------------------
// Note-off is ignored on purpose: hits are one-shots, and sustain is a `decay_ms` mod armed
// on the keys context. The polling thread owns the device; the studio holds only the state.
fn start_midi(st: Arc<Studio>, id: u32) -> Result<String, String> {
    let mut dev = Midi::open(id)?;
    let name = dev.name.clone();
    *st.midi.lock().unwrap() = Some(MidiState { id, name: name.clone(), mode: KeysMode::Drums, track: 0 });
    std::thread::spawn(move || {
        loop {
            for e in dev.poll() {
                if !e.on { continue; }
                // Snapshot the context under a brief lock, then do the work unlocked.
                let state = match st.midi.lock().unwrap().clone() { Some(s) => s, None => return };
                let armed = st.keys_armed.lock().unwrap().clone();
                let hit = match state.mode {
                    KeysMode::Drums => {
                        let slot = st.song.lock().unwrap().tracks.get(state.track).and_then(|t| t.slot_for_note(e.note));
                        match slot { Some(s) => Hit { slot: s, vel: e.vel.clamp(1, 127), mods: armed }, None => continue }
                    }
                    KeysMode::Chromatic { slot, root } => {
                        // The note's pitch wins over an armed pitch; everything else armed rides along.
                        let pitch = 2f32.powf((e.note as f32 - root as f32) / 12.0);
                        let mods = armed.over(&Mods { pitch: Some(pitch), ..Default::default() });
                        Hit { slot, vel: e.vel.clamp(1, 127), mods }
                    }
                };
                record_hit(&st, state.track, hit);
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    });
    Ok(name)
}

// ---- tools ------------------------------------------------------------------------------
fn tools() -> Value {
    let mods_schema = json!({"type":"object","description":"per-hit overrides; omit a key to keep the voice default",
        "properties":{
            "pitch":{"type":"number","description":"speed multiplier, 0.5 octave down, 2 up"},
            "drive":{"type":"number","description":"tanh saturation: 1 gentle, 4 wall, 10 fuzz"},
            "crush":{"type":"integer","description":"bit depth 1-15: 8 gritty, 4 destroyed"},
            "rev":{"type":"boolean"},
            "gain":{"type":"number","description":"linear level"},
            "decay_ms":{"type":"number","description":"force fade to -60 dB by this many ms"}}});
    let hit_schema = json!({"type":"object","required":["slot","vel"],"properties":{
        "slot":{"type":"integer","description":"kit slot 0-8"},
        "vel":{"type":"integer","description":"1-127"},
        "mods":mods_schema}});
    let t = |name: &str, desc: &str, props: Value, req: Vec<&str>| json!({
        "name": name, "description": desc,
        "inputSchema": {"type":"object","properties":props,"required":req}});
    json!([
        t("status", "Transport, song summary, pads and record state. Call this first.", json!({}), vec![]),
        t("song_get", "The whole song as JSON: bpm and tracks with cells of hits.", json!({}), vec![]),
        t("song_set", "Replace the whole song. Shape as returned by song_get.", json!({"song":{"type":"object"}}), vec!["song"]),
        t("song_save", "Write the song to a JSON file.", json!({"path":{"type":"string"}}), vec!["path"]),
        t("song_load", "Load a song JSON file and make it current.", json!({"path":{"type":"string"}}), vec!["path"]),
        t("track_add", "Add a track. len is in sixteenth steps: 16 = 4/4 bar, 12 = 3/4, 14 = 7/8. pads maps the 4 plates to kit slots for this track; keys maps MIDI notes to slots (drum mode).",
          json!({"name":{"type":"string"},"len":{"type":"integer"},"pads":{"type":"array","items":{"type":"integer"},"minItems":4,"maxItems":4},
                 "keys":{"type":"object","additionalProperties":{"type":"integer"}}}), vec!["name","len"]),
        t("track_set", "Edit a track's name, len, pads, keys or mute.",
          json!({"track":{"type":"integer"},"name":{"type":"string"},"len":{"type":"integer"},"pads":{"type":"array","items":{"type":"integer"}},
                 "keys":{"type":"object","additionalProperties":{"type":"integer"}},"mute":{"type":"boolean"}}), vec!["track"]),
        t("track_clear", "Remove every hit from a track.", json!({"track":{"type":"integer"}}), vec!["track"]),
        t("hit_set", "Set the hits on one cell (replaces). Empty list clears it.",
          json!({"track":{"type":"integer"},"step":{"type":"integer"},"hits":{"type":"array","items":hit_schema}}), vec!["track","step","hits"]),
        t("hit_add", "Add one hit to a cell, replacing any hit already there on the same slot.",
          json!({"track":{"type":"integer"},"step":{"type":"integer"},"hit":hit_schema}), vec!["track","step","hit"]),
        t("fill", "Write a hit on every step matching a pattern string over the track: '.'=skip, 'x'=vel 90, 'X'=vel 127, '1'-'9'=that ninth. mods apply to every hit written.",
          json!({"track":{"type":"integer"},"slot":{"type":"integer"},"pattern":{"type":"string"},"mods":mods_schema}), vec!["track","slot","pattern"]),
        t("mods_apply", "ANY MODIFIER, ANY NOTE: merge mods onto hits in a step range of a track (inclusive). Omit slot for all slots.",
          json!({"track":{"type":"integer"},"from":{"type":"integer"},"to":{"type":"integer"},"slot":{"type":"integer"},"mods":mods_schema}), vec!["track","from","to","mods"]),
        t("play", "Start (or resume) the transport. stop rewinds to step 0.", json!({}), vec![]),
        t("stop", "Stop and rewind to step 0.", json!({}), vec![]),
        t("bpm", "Set the tempo.", json!({"bpm":{"type":"number"}}), vec!["bpm"]),
        t("master", "Master gain before the soft clip. Default 2.0.", json!({"gain":{"type":"number"}}), vec!["gain"]),
        t("trigger", "Play one hit right now, off the grid.", json!({"hit":hit_schema}), vec!["hit"]),
        t("kit_load", "Load a .kit file (path relative to the project).", json!({"path":{"type":"string"}}), vec!["path"]),
        t("kit_info", "Slots, labels and sample lengths of the loaded kit.", json!({}), vec![]),
        t("render", "Render the song to a 16-bit WAV. steps defaults to one full polymeter cycle.",
          json!({"path":{"type":"string"},"steps":{"type":"integer"}}), vec!["path"]),
        t("pads_open", "Open the pad controller (default COM5), calibrate the untouched baseline, start play-through. Hands off the pads for 3 s.",
          json!({"port":{"type":"string"}}), vec![]),
        t("pads_learn", "Ask for three taps per pad and derive each pad's threshold and gain. Returns immediately; status.pads.learned fills in (90 s timeout).", json!({}), vec![]),
        t("pads_context", "Which track the pads play into (its pad map picks the sounds).", json!({"track":{"type":"integer"}}), vec!["track"]),
        t("record", "Recording on/off: while playing, pad hits are quantised into the context track with the armed mods.",
          json!({"on":{"type":"boolean"}}), vec!["on"]),
        t("arm", "Arm mods for every hit recorded from now on (any modifier, any note). Empty object disarms. target picks the pads (default) or the keys context.",
          json!({"mods":mods_schema,"target":{"type":"string","enum":["pads","keys"]}}), vec!["mods"]),
        t("midi_list", "MIDI input devices as id + name (the Casio over USB shows up here).", json!({}), vec![]),
        t("midi_open", "Open a MIDI input (default id 0) and start play-through. Keys get their own context: see midi_context.",
          json!({"id":{"type":"integer"}}), vec![]),
        t("midi_context", "Where the keyboard plays and how. drums: notes map to kit slots through the track's key map (GM layout by default: 36 kick, 38 snare, 42 hat, 49 crash...). chromatic: every key plays one slot pitched by note, root = the note that plays it unpitched (default 36). Note-off is ignored: sustain is arm {decay_ms} with target keys.",
          json!({"track":{"type":"integer"},"mode":{"type":"string","enum":["drums","chromatic"]},"slot":{"type":"integer"},"root":{"type":"integer"}}), vec!["track","mode"]),
    ])
}

fn arg<'a>(a: &'a Value, k: &str) -> Option<&'a Value> { a.get(k) }
fn usize_arg(a: &Value, k: &str) -> Result<usize, String> {
    arg(a, k).and_then(|v| v.as_u64()).map(|v| v as usize).ok_or(format!("missing or bad '{k}'"))
}
fn parse_keys(v: Option<&Value>) -> Option<std::collections::BTreeMap<u8, usize>> {
    let obj = v?.as_object()?;
    Some(obj.iter().filter_map(|(k, s)| Some((k.parse::<u8>().ok()?, s.as_u64()? as usize))).collect())
}

fn call(st: &Arc<Studio>, name: &str, a: &Value) -> Result<Value, String> {
    let ok = |s: String| Ok(json!({"ok": s}));
    match name {
        "status" => {
            let s = st.song.lock().unwrap();
            let pads = st.pads.lock().unwrap();
            Ok(json!({
                "playing": st.playing.load(Ordering::Relaxed),
                "step": st.step.load(Ordering::Relaxed),
                "bpm": s.bpm,
                "kit": *st.kit_path.lock().unwrap(),
                "tracks": s.tracks.iter().enumerate().map(|(i,t)| json!({
                    "index": i, "name": t.name, "len": t.len, "pads": t.pads, "mute": t.mute,
                    "hits": t.cells.iter().map(|c| c.len()).sum::<usize>()})).collect::<Vec<_>>(),
                "cycle_steps": s.cycle_steps(),
                "context": st.context.load(Ordering::Relaxed),
                "recording": st.recording.load(Ordering::Relaxed),
                "armed": *st.armed.lock().unwrap(),
                "pads": pads.as_ref().map(|p| json!({"names": p.names, "learned": p.learned})),
                "midi": st.midi.lock().unwrap().as_ref().map(|m| json!({
                    "id": m.id, "name": m.name, "track": m.track,
                    "mode": match m.mode { KeysMode::Drums => "drums", KeysMode::Chromatic{..} => "chromatic" },
                    "slot": match m.mode { KeysMode::Chromatic{slot,..} => Some(slot), _ => None },
                    "root": match m.mode { KeysMode::Chromatic{root,..} => Some(root), _ => None }})),
                "keys_armed": *st.keys_armed.lock().unwrap(),
            }))
        }
        "song_get" => Ok(serde_json::to_value(&*st.song.lock().unwrap()).unwrap()),
        "song_set" => {
            let mut s: Song = serde_json::from_value(arg(a, "song").cloned().ok_or("missing 'song'")?).map_err(|e| e.to_string())?;
            s.normalise();
            *st.song.lock().unwrap() = s;
            st.push_song()?; ok("song replaced".into())
        }
        "song_save" => {
            let p = arg(a, "path").and_then(Value::as_str).ok_or("missing 'path'")?;
            let text = serde_json::to_string_pretty(&*st.song.lock().unwrap()).unwrap();
            std::fs::write(p, text).map_err(|e| format!("{p}: {e}"))?;
            ok(format!("saved {p}"))
        }
        "song_load" => {
            let p = arg(a, "path").and_then(Value::as_str).ok_or("missing 'path'")?;
            let text = std::fs::read_to_string(p).map_err(|e| format!("{p}: {e}"))?;
            let mut s: Song = serde_json::from_str(&text).map_err(|e| format!("{p}: {e}"))?;
            s.normalise();
            *st.song.lock().unwrap() = s;
            st.push_song()?; ok(format!("loaded {p}"))
        }
        "track_add" => {
            let name = arg(a, "name").and_then(Value::as_str).ok_or("missing 'name'")?;
            let len = usize_arg(a, "len")?;
            let mut t = Track::new(name, len);
            if let Some(p) = arg(a, "pads").and_then(Value::as_array) {
                for (i, v) in p.iter().take(4).enumerate() { t.pads[i] = v.as_u64().unwrap_or(0) as usize; }
            }
            if let Some(k) = parse_keys(arg(a, "keys")) { t.keys = k; }
            let idx = { let mut s = st.song.lock().unwrap(); s.tracks.push(t); s.tracks.len() - 1 };
            st.push_song()?; Ok(json!({"ok": "track added", "index": idx}))
        }
        "track_set" => {
            let i = usize_arg(a, "track")?;
            { let mut s = st.song.lock().unwrap();
              let t = s.tracks.get_mut(i).ok_or(format!("no track {i}"))?;
              if let Some(n) = arg(a, "name").and_then(Value::as_str) { t.name = n.into(); }
              if let Some(l) = arg(a, "len").and_then(Value::as_u64) { t.len = l as usize; }
              if let Some(p) = arg(a, "pads").and_then(Value::as_array) {
                  for (k, v) in p.iter().take(4).enumerate() { t.pads[k] = v.as_u64().unwrap_or(0) as usize; } }
              if let Some(k) = parse_keys(arg(a, "keys")) { t.keys = k; }
              if let Some(m) = arg(a, "mute").and_then(Value::as_bool) { t.mute = m; }
              t.normalise(); }
            st.push_song()?; ok(format!("track {i} updated"))
        }
        "track_clear" => {
            let i = usize_arg(a, "track")?;
            { let mut s = st.song.lock().unwrap();
              let t = s.tracks.get_mut(i).ok_or(format!("no track {i}"))?;
              for c in &mut t.cells { c.clear(); } }
            st.push_song()?; ok(format!("track {i} cleared"))
        }
        "hit_set" | "hit_add" => {
            let i = usize_arg(a, "track")?; let step = usize_arg(a, "step")?;
            { let mut s = st.song.lock().unwrap();
              let t = s.tracks.get_mut(i).ok_or(format!("no track {i}"))?;
              t.normalise();
              let len = t.len;
              let cell = t.cells.get_mut(step).ok_or(format!("step {step} out of range 0-{}", len - 1))?;
              if name == "hit_set" {
                  let hits: Vec<Hit> = serde_json::from_value(arg(a, "hits").cloned().ok_or("missing 'hits'")?).map_err(|e| e.to_string())?;
                  *cell = hits;
              } else {
                  let h: Hit = serde_json::from_value(arg(a, "hit").cloned().ok_or("missing 'hit'")?).map_err(|e| e.to_string())?;
                  cell.retain(|x| x.slot != h.slot);
                  cell.push(h);
              } }
            st.push_song()?; ok(format!("track {i} step {step} updated"))
        }
        "fill" => {
            let i = usize_arg(a, "track")?; let slot = usize_arg(a, "slot")?;
            let pat = arg(a, "pattern").and_then(Value::as_str).ok_or("missing 'pattern'")?;
            let mods: Mods = arg(a, "mods").cloned().map(serde_json::from_value).transpose().map_err(|e| e.to_string())?.unwrap_or_default();
            let mut written = 0;
            { let mut s = st.song.lock().unwrap();
              let t = s.tracks.get_mut(i).ok_or(format!("no track {i}"))?;
              t.normalise();
              for (k, c) in pat.chars().filter(|c| !c.is_whitespace()).enumerate() {
                  if k >= t.len { break; }
                  let vel = match c { '.' | '-' => 0, 'x' => 90, 'X' => 127, '1'..='9' => ((c as u8 - b'0') as u32 * 127 / 9) as u8, _ => 0 };
                  if vel == 0 { continue; }
                  t.cells[k].retain(|x| x.slot != slot);
                  t.cells[k].push(Hit { slot, vel, mods: mods.clone() });
                  written += 1;
              } }
            st.push_song()?; ok(format!("wrote {written} hits on track {i} slot {slot}"))
        }
        "mods_apply" => {
            let i = usize_arg(a, "track")?; let from = usize_arg(a, "from")?; let to = usize_arg(a, "to")?;
            let slot = arg(a, "slot").and_then(Value::as_u64).map(|v| v as usize);
            let mods: Mods = serde_json::from_value(arg(a, "mods").cloned().ok_or("missing 'mods'")?).map_err(|e| e.to_string())?;
            let mut n = 0;
            { let mut s = st.song.lock().unwrap();
              let t = s.tracks.get_mut(i).ok_or(format!("no track {i}"))?;
              t.normalise();
              for step in from..=to.min(t.len.saturating_sub(1)) {
                  for h in &mut t.cells[step] {
                      if slot.map_or(true, |sl| sl == h.slot) { h.mods = h.mods.over(&mods); n += 1; }
                  } } }
            st.push_song()?; ok(format!("mods applied to {n} hits"))
        }
        "play" => { st.send(Cmd::Play)?; ok("playing".into()) }
        "stop" => { st.send(Cmd::Stop)?; ok("stopped".into()) }
        "bpm" => {
            let b = arg(a, "bpm").and_then(Value::as_f64).ok_or("missing 'bpm'")? as f32;
            { let mut s = st.song.lock().unwrap(); s.bpm = b; s.normalise(); }
            st.push_song()?; ok(format!("bpm {b}"))
        }
        "master" => {
            let g = arg(a, "gain").and_then(Value::as_f64).ok_or("missing 'gain'")? as f32;
            st.send(Cmd::Master(g))?; ok(format!("master {g}"))
        }
        "trigger" => {
            let h: Hit = serde_json::from_value(arg(a, "hit").cloned().ok_or("missing 'hit'")?).map_err(|e| e.to_string())?;
            st.send(Cmd::Trigger(h))?; ok("triggered".into())
        }
        "kit_load" => {
            let p = arg(a, "path").and_then(Value::as_str).ok_or("missing 'path'")?;
            let k = Arc::new(Kit::load(Path::new(p))?);
            let running = st.kit.read().unwrap().rate;
            if k.rate != running { return Err(format!("kit rate {} differs from the running {running} — restart the server with this kit", k.rate)); }
            st.send(Cmd::SetKit(k.clone()))?;
            *st.kit.write().unwrap() = k.clone();
            *st.kit_path.lock().unwrap() = p.to_string();
            ok(format!("kit loaded: {} ({} voices)", k.name, k.voices.iter().filter(|v| v.is_some()).count()))
        }
        "kit_info" => {
            let k = st.kit.read().unwrap().clone();
            Ok(json!({"name": k.name, "rate": k.rate,
                "slots": k.voices.iter().enumerate().map(|(i,v)| json!({"slot": i,
                    "label": v.as_ref().map(|v| v.label.clone()),
                    "ms": v.as_ref().map(|v| v.mono.len() as f32 * 1000.0 / k.rate as f32)})).collect::<Vec<_>>()}))
        }
        "render" => {
            let p = arg(a, "path").and_then(Value::as_str).ok_or("missing 'path'")?;
            let song = st.song.lock().unwrap().clone();
            let kit = st.kit.read().unwrap().clone();
            let steps = arg(a, "steps").and_then(Value::as_u64).map(|v| v as usize).unwrap_or_else(|| song.cycle_steps());
            let data = Transport::render(kit.clone(), song, steps);
            let peak = data.iter().fold(0f32, |m, s| m.max(s.abs()));
            let w = crate::wav::Wav { rate: kit.rate, channels: 1, data };
            std::fs::write(p, w.to_bytes()).map_err(|e| format!("{p}: {e}"))?;
            Ok(json!({"ok": format!("rendered {p}"), "seconds": w.frames() as f32 / kit.rate as f32, "peak": peak, "steps": steps}))
        }
        "pads_open" => {
            let port = arg(a, "port").and_then(Value::as_str).unwrap_or("COM5").to_string();
            if st.pads.lock().unwrap().is_some() { return Err("pads already open".into()); }
            start_pads(st.clone(), port.clone())?;
            ok(format!("pads open on {port}, calibrated; call pads_learn and tap each pad 3 times"))
        }
        "pads_learn" => {
            let tx = st.pads.lock().unwrap().as_ref().map(|p| p.tx.clone()).ok_or("pads not open")?;
            tx.send("learn".into()).map_err(|e| e.to_string())?;
            ok("learning: tap each pad 3 times; status.pads.learned fills in".into())
        }
        "pads_context" => {
            let i = usize_arg(a, "track")?;
            let n = st.song.lock().unwrap().tracks.len();
            if i >= n { return Err(format!("no track {i}")); }
            st.context.store(i as u32, Ordering::Relaxed); ok(format!("pads now play into track {i}"))
        }
        "record" => {
            let on = arg(a, "on").and_then(Value::as_bool).ok_or("missing 'on'")?;
            st.recording.store(on, Ordering::Relaxed); ok(format!("recording {}", if on {"on"} else {"off"}))
        }
        "arm" => {
            let m: Mods = serde_json::from_value(arg(a, "mods").cloned().ok_or("missing 'mods'")?).map_err(|e| e.to_string())?;
            let target = arg(a, "target").and_then(Value::as_str).unwrap_or("pads");
            match target {
                "pads" => *st.armed.lock().unwrap() = m.clone(),
                "keys" => *st.keys_armed.lock().unwrap() = m.clone(),
                other => return Err(format!("bad target '{other}': pads|keys")),
            }
            Ok(json!({"ok": "armed", "target": target, "mods": m}))
        }
        "midi_list" => Ok(json!({"devices": Midi::list().into_iter().map(|(id, name)| json!({"id": id, "name": name})).collect::<Vec<_>>()})),
        "midi_open" => {
            let id = arg(a, "id").and_then(Value::as_u64).unwrap_or(0) as u32;
            if st.midi.lock().unwrap().is_some() { return Err("midi already open".into()); }
            let name = start_midi(st.clone(), id)?;
            ok(format!("midi open: {id} {name}; drums mode into track 0 — set midi_context"))
        }
        "midi_context" => {
            let track = usize_arg(a, "track")?;
            let n = st.song.lock().unwrap().tracks.len();
            if track >= n { return Err(format!("no track {track}")); }
            let mode = match arg(a, "mode").and_then(Value::as_str).ok_or("missing 'mode'")? {
                "drums" => KeysMode::Drums,
                "chromatic" => KeysMode::Chromatic {
                    slot: arg(a, "slot").and_then(Value::as_u64).unwrap_or(0) as usize,
                    root: arg(a, "root").and_then(Value::as_u64).unwrap_or(36) as u8,
                },
                other => return Err(format!("bad mode '{other}': drums|chromatic")),
            };
            let mut g = st.midi.lock().unwrap();
            let s = g.as_mut().ok_or("midi not open")?;
            s.track = track; s.mode = mode.clone();
            ok(format!("keys -> track {track}, {mode:?}"))
        }
        other => Err(format!("unknown tool {other}")),
    }
}

// ---- JSON-RPC ---------------------------------------------------------------------------
/// Methods that need no studio state: testable without an audio device.
fn dispatch_meta(method: &str) -> Option<Result<Value, (i64, String)>> {
    match method {
        "initialize" => Some(Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "toasteddrums", "version": env!("CARGO_PKG_VERSION")}}))),
        "ping" => Some(Ok(json!({}))),
        "tools/list" => Some(Ok(json!({"tools": tools()}))),
        _ => None,
    }
}

fn dispatch(st: &Arc<Studio>, method: &str, params: &Value) -> Result<Value, (i64, String)> {
    if let Some(r) = dispatch_meta(method) { return r; }
    match method {
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let empty = json!({});
            let args = params.get("arguments").unwrap_or(&empty);
            Ok(match call(st, name, args) {
                Ok(v) => json!({"content": [{"type": "text", "text": serde_json::to_string_pretty(&v).unwrap()}], "isError": false}),
                Err(e) => json!({"content": [{"type": "text", "text": e}], "isError": true}),
            })
        }
        _ => Err((-32601, format!("method not found: {method}"))),
    }
}

fn envelope(id: Option<Value>, r: Result<Value, (i64, String)>) -> Value {
    match r {
        Ok(v) => json!({"jsonrpc": "2.0", "id": id, "result": v}),
        Err((c, m)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": c, "message": m}}),
    }
}

/// One JSON-RPC line loop over any reader/writer pair: stdio for Claude, a TCP stream for
/// the UI. Notifications get no reply; malformed JSON gets a -32700.
fn serve_lines(st: &Arc<Studio>, r: impl BufRead, mut w: impl Write) {
    for line in r.lines() {
        let line = match line { Ok(l) => l, Err(_) => break };
        if line.trim().is_empty() { continue; }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let _ = writeln!(w, "{}", envelope(None, Err((-32700, e.to_string()))));
                continue;
            }
        };
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let empty = json!({});
        let params = req.get("params").unwrap_or(&empty);
        if method.starts_with("notifications/") { continue; }
        let reply = envelope(id, dispatch(st, method, params));
        if writeln!(w, "{reply}").is_err() { break; }
        let _ = w.flush();
    }
}

/// The UI door: same verbs, same dispatch, over localhost. One thread per connection.
fn serve_tcp(st: Arc<Studio>, port: u16) -> Result<(), String> {
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| format!("tcp {port}: {e}"))?;
    eprintln!("toasteddrums mcp: ui door on 127.0.0.1:{port}");
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let stream = match conn { Ok(s) => s, Err(_) => continue };
            let reader = match stream.try_clone() { Ok(r) => BufReader::new(r), Err(_) => continue };
            let st = st.clone();
            std::thread::spawn(move || serve_lines(&st, reader, stream));
        }
    });
    Ok(())
}

pub fn serve(kit_path: &str, pads_port: Option<&str>, door_port: Option<u16>) -> Result<(), String> {
    let kit = Arc::new(Kit::load(Path::new(kit_path))?);
    let song = Song::empty(120.0);
    let playing = Arc::new(AtomicBool::new(false));
    let step = Arc::new(AtomicU64::new(0));
    let phase = Arc::new(AtomicU32::new(0));
    let (out, to_audio) = start_audio(kit.clone(), song.clone(), playing.clone(), step.clone(), phase.clone())?;

    let st = Arc::new(Studio {
        kit: RwLock::new(kit), kit_path: Mutex::new(kit_path.into()), song: Mutex::new(song),
        to_audio: Mutex::new(to_audio), playing, step, phase_milli: phase,
        context: AtomicU32::new(0), recording: AtomicBool::new(false),
        armed: Mutex::new(Mods::default()), pads: Mutex::new(None),
        midi: Mutex::new(None), keys_armed: Mutex::new(Mods::default()), _out: out,
    });
    if let Some(p) = pads_port { if let Err(e) = start_pads(st.clone(), p.into()) { eprintln!("pads: {e} (use pads_open later)"); } }
    if let Some(p) = door_port { serve_tcp(st.clone(), p)?; }
    eprintln!("toasteddrums mcp: kit {kit_path}, stdio ready");

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve_lines(&st, stdin.lock(), stdout.lock());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_have_unique_names_and_schemas() {
        let t = tools();
        let arr = t.as_array().unwrap();
        let mut names: Vec<&str> = arr.iter().map(|x| x["name"].as_str().unwrap()).collect();
        let n = names.len();
        names.sort(); names.dedup();
        assert_eq!(names.len(), n, "duplicate tool names");
        for x in arr {
            assert_eq!(x["inputSchema"]["type"], "object", "{} lacks an object inputSchema", x["name"]);
            assert!(x["description"].as_str().map_or(false, |d| !d.is_empty()));
        }
    }

    #[test]
    fn envelope_shapes_results_and_errors() {
        let ok = envelope(Some(json!(7)), Ok(json!({"a": 1})));
        assert_eq!(ok["id"], 7);
        assert_eq!(ok["result"]["a"], 1);
        assert!(ok.get("error").is_none());
        let err = envelope(None, Err((-32601, "nope".into())));
        assert_eq!(err["error"]["code"], -32601);
        assert!(err["id"].is_null());
    }

    #[test]
    fn meta_methods_need_no_studio() {
        let init = dispatch_meta("initialize").unwrap().unwrap();
        assert_eq!(init["protocolVersion"], "2024-11-05");
        assert_eq!(init["serverInfo"]["name"], "toasteddrums");
        assert!(dispatch_meta("ping").unwrap().is_ok());
        assert!(dispatch_meta("tools/list").unwrap().unwrap()["tools"].is_array());
        assert!(dispatch_meta("tools/call").is_none(), "tools/call needs the studio");
    }
}
