//! ui — the operator's door: an eframe window laid out like a tracker, with the 808's
//! front panel around it.
//!
//! WHY A CLIENT. The UI never touches the song directly. It speaks the same JSON-RPC verbs
//! Claude does, over the localhost TCP door of `mcp`, so there is exactly one song and one
//! engine no matter who is editing. `ui` with nothing listening starts the engine in this
//! process and connects to it the same way -- the code path is identical either way.
//!
//! THREADS. A worker thread owns the socket. It drains the request queue, then polls
//! `status` every 30 ms and `song_get` only when the song version moved, and publishes a
//! snapshot the paint thread clones under a short lock. Every edit is ALSO applied to the
//! local snapshot at once, so a drag or a keypress never waits for the round trip; the next
//! poll overwrites it with the canonical song, which is the same thing. A poll that raced
//! with a queued edit is discarded rather than let it flicker the edit away for 30 ms.
//!
//! LAYOUT. Rows are steps, columns are the nine kit slots, as in a drum tracker. A cell is
//! `VV FFFFFF`: velocity in hex, then one flag per mod (P D C R G L = pitch, drive, crush,
//! reverse, gain, decay), so "any modifier, any note" is visible at a glance. Around it, the
//! 808: the instrument strip with TUNE / LEVEL / DECAY per voice and tap buttons, the
//! sixteen step buttons in red / orange / yellow / white with the running light, START /
//! STOP, TEMPO, ACCENT, and pattern select (a track is a pattern with its own length).
//!
//! KEYS (when no text field has focus)
//!   arrows / PgUp PgDn / Home End   move the cursor          Tab, Shift+Tab   next / prev track
//!   1-9        write a hit at that ninth (9 = accent)         0 Del Backspace  clear the cell
//!   Enter      toggle a hit (x, or X with ACCENT lit)         a                accent on / off
//!   r          reverse on / off      - =   pitch down / up a semitone
//!   z x c v b n m , .   tap slots 0-8 (records while REC and playing, like TAP WRITE)
//!   Space      START / STOP

use crate::song::{quantise, Hit, Mods, Song, Track};
use eframe::egui::{self, Align, Align2, Color32, FontId, Key, Margin, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, TextStyle, Vec2};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const DEFAULT_DOOR: u16 = 4242;

// ---- palette ---------------------------------------------------------------------------
const BG: Color32 = Color32::from_rgb(0x12, 0x12, 0x14);
const PANEL: Color32 = Color32::from_rgb(0x1a, 0x1a, 0x1d);
const BEAT: Color32 = Color32::from_rgb(0x20, 0x20, 0x25);
const PLAYROW: Color32 = Color32::from_rgb(0x3a, 0x2c, 0x12);
const GRID: Color32 = Color32::from_rgb(0x2a, 0x2a, 0x30);
const DIM: Color32 = Color32::from_rgb(0x44, 0x44, 0x4c);
const NUM: Color32 = Color32::from_rgb(0x8a, 0x8a, 0x92);
const TEXT: Color32 = Color32::from_rgb(0xd8, 0xd4, 0xc8);
const ORANGE: Color32 = Color32::from_rgb(0xf2, 0x8c, 0x28);
const LED_ON: Color32 = Color32::from_rgb(0xff, 0x30, 0x20);
const LED_OFF: Color32 = Color32::from_rgb(0x50, 0x14, 0x10);
/// The 808's four button colours, one per beat of the bar.
const STEP_COLORS: [Color32; 4] = [
    Color32::from_rgb(0xf0, 0x3a, 0x2e), Color32::from_rgb(0xf2, 0x8c, 0x28),
    Color32::from_rgb(0xf2, 0xd2, 0x1f), Color32::from_rgb(0xec, 0xe9, 0xdc),
];

const ROW_H: f32 = 18.0;
const CELL_W: f32 = 72.0;
const NUM_W: f32 = 34.0;
const FONT: f32 = 12.0;

// ---- the door client ---------------------------------------------------------------------
struct Door { r: BufReader<TcpStream>, w: TcpStream, id: u64 }

impl Door {
    fn connect(port: u16) -> std::io::Result<Door> {
        let w = TcpStream::connect(("127.0.0.1", port))?;
        w.set_nodelay(true)?;
        let r = BufReader::new(w.try_clone()?);
        Ok(Door { r, w, id: 0 })
    }
    /// One `tools/call`, synchronously. The reply's text is the tool's JSON, parsed back.
    fn call(&mut self, tool: &str, args: Value) -> Result<Value, String> {
        self.id += 1;
        let req = json!({"jsonrpc":"2.0","id":self.id,"method":"tools/call",
                         "params":{"name":tool,"arguments":args}});
        writeln!(self.w, "{req}").map_err(|e| format!("door: {e}"))?;
        let mut line = String::new();
        if self.r.read_line(&mut line).map_err(|e| format!("door: {e}"))? == 0 { return Err("door closed".into()); }
        let v: Value = serde_json::from_str(&line).map_err(|e| format!("door: bad reply: {e}"))?;
        if let Some(e) = v.get("error") { return Err(e["message"].as_str().unwrap_or("error").to_string()); }
        let res = &v["result"];
        let text = res["content"][0]["text"].as_str().unwrap_or("");
        if res["isError"].as_bool().unwrap_or(false) { return Err(text.to_string()); }
        serde_json::from_str(text).map_err(|e| format!("{tool}: {e}"))
    }
}

// ---- shared snapshot -----------------------------------------------------------------------
#[derive(Clone, Default)]
struct Status {
    playing: bool, step: u64, phase: f32, version: u64, master: f32, bpm: f32,
    kit: String, context: usize, recording: bool, armed: Mods, keys_armed: Mods,
    pads: Option<Value>, midi: Option<Value>,
}

impl Status {
    fn from(v: &Value) -> Status {
        let f = |k: &str| v[k].as_f64().unwrap_or(0.0) as f32;
        let opt = |k: &str| v.get(k).filter(|x| !x.is_null()).cloned();
        Status {
            playing: v["playing"].as_bool().unwrap_or(false),
            step: v["step"].as_u64().unwrap_or(0),
            phase: f("phase"), version: v["version"].as_u64().unwrap_or(0), master: f("master"), bpm: f("bpm"),
            kit: v["kit"].as_str().unwrap_or("").to_string(),
            context: v["context"].as_u64().unwrap_or(0) as usize,
            recording: v["recording"].as_bool().unwrap_or(false),
            armed: serde_json::from_value(v["armed"].clone()).unwrap_or_default(),
            keys_armed: serde_json::from_value(v["keys_armed"].clone()).unwrap_or_default(),
            pads: opt("pads"), midi: opt("midi"),
        }
    }
}

#[derive(Clone)]
struct Slot { label: String, color: Color32 }

#[derive(Clone)]
struct Shared {
    st: Status,
    song: Song,
    slots: Vec<Slot>,
    kits: Vec<String>,
    kit_name: String,
    midi_devs: Vec<(u32, String)>,
    log: Vec<String>,
    connected: bool,
    /// false until the first status arrives: widgets must not sync from, or write, zeros
    loaded: bool,
}

impl Shared {
    fn log(&mut self, s: impl Into<String>) {
        self.log.push(s.into());
        if self.log.len() > 200 { self.log.remove(0); }
    }
}

struct Req { tool: String, args: Value }

fn slots_from(v: &Value) -> (String, Vec<Slot>) {
    let name = v["name"].as_str().unwrap_or("").to_string();
    let slots = v["slots"].as_array().map(|a| a.iter().map(|s| {
        let c = s["color"].as_array().map(|c| c.iter().map(|x| x.as_u64().unwrap_or(0) as u8).collect::<Vec<_>>()).unwrap_or_default();
        Slot {
            label: s["label"].as_str().unwrap_or("").to_string(),
            color: if c.len() == 3 { Color32::from_rgb(c[0], c[1], c[2]) } else { DIM },
        }
    }).collect()).unwrap_or_default();
    (name, slots)
}

/// Owns the socket. Requests first, then status, then the song if it moved.
fn worker(mut door: Door, rx: Receiver<Req>, sh: Arc<Mutex<Shared>>, ctx: egui::Context) {
    let mut seen_version = u64::MAX;
    let mut last_kit = String::new();
    let mut pending: Vec<Req> = Vec::new();
    loop {
        let mut reqs = std::mem::take(&mut pending);
        if reqs.is_empty() {
            match rx.recv_timeout(Duration::from_millis(30)) {
                Ok(r) => reqs.push(r),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        while let Ok(r) = rx.try_recv() { reqs.push(r); }
        let mut refresh_midi = false;
        for r in reqs {
            if r.tool == "midi_list" || r.tool == "midi_open" { refresh_midi = true; }
            match door.call(&r.tool, r.args) {
                Ok(v) => if let Some(s) = v.get("ok").and_then(Value::as_str) { sh.lock().unwrap().log(s); },
                Err(e) => sh.lock().unwrap().log(format!("{}: {e}", r.tool)),
            }
        }
        let st = match door.call("status", json!({})) {
            Ok(v) => Status::from(&v),
            Err(e) => {
                let mut g = sh.lock().unwrap();
                g.connected = false; g.log(format!("lost the door: {e}"));
                ctx.request_repaint();
                return;
            }
        };
        let song: Option<Song> = if st.version != seen_version {
            door.call("song_get", json!({})).ok().and_then(|v| serde_json::from_value(v).ok())
        } else { None };
        let kit = if st.kit != last_kit {
            let info = door.call("kit_info", json!({})).ok().map(|v| slots_from(&v));
            let kits = door.call("kit_list", json!({})).ok().and_then(|v| v["kits"].as_array().map(|a|
                a.iter().filter_map(|k| k.as_str().map(String::from)).collect::<Vec<_>>()));
            last_kit = st.kit.clone();
            Some((info, kits))
        } else { None };
        let midi = if refresh_midi || seen_version == u64::MAX {
            door.call("midi_list", json!({})).ok().and_then(|v| v["devices"].as_array().map(|a|
                a.iter().map(|d| (d["id"].as_u64().unwrap_or(0) as u32, d["name"].as_str().unwrap_or("").to_string())).collect()))
        } else { None };
        // An edit queued while we were polling has already been applied locally; writing this
        // (older) song over it would undo it for one cycle. Hold the edit, skip the song.
        let raced = match rx.try_recv() { Ok(r) => { pending.push(r); true } Err(_) => false };
        {
            let mut g = sh.lock().unwrap();
            g.st = st.clone();
            g.connected = true;
            g.loaded = true;
            if let (Some(s), false) = (song, raced) { g.song = s; seen_version = st.version; }
            if let Some((info, kits)) = kit {
                if let Some((n, s)) = info { g.kit_name = n; g.slots = s; }
                if let Some(k) = kits { g.kits = k; }
            }
            if let Some(m) = midi { g.midi_devs = m; }
        }
        ctx.request_repaint();
    }
}

// ---- the app ------------------------------------------------------------------------------
struct App {
    sh: Arc<Mutex<Shared>>,
    tx: Sender<Req>,
    track: usize,
    cur_step: usize,
    cur_slot: usize,
    accent: bool,
    follow: bool,
    bpm_local: f32,
    master_local: f32,
    song_path: String,
    wav_path: String,
    pads_port: String,
    new_len: usize,
    midi_pick: u32,
    /// the name field being typed into, and which track it belongs to; synced from the
    /// song only while the field is not focused, or every frame would eat the keystrokes
    name_local: String,
    name_track: Option<usize>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, door: Door) -> App {
        let ctx = &cc.egui_ctx;
        let mut vis = egui::Visuals::dark();
        vis.panel_fill = PANEL;
        vis.window_fill = PANEL;
        vis.extreme_bg_color = BG;
        vis.faint_bg_color = BEAT;
        vis.override_text_color = Some(TEXT);
        vis.selection.bg_fill = ORANGE.gamma_multiply(0.55);
        vis.widgets.inactive.bg_fill = GRID;
        vis.widgets.inactive.weak_bg_fill = GRID;
        vis.widgets.hovered.bg_fill = DIM;
        vis.widgets.hovered.weak_bg_fill = DIM;
        vis.widgets.active.bg_fill = ORANGE.gamma_multiply(0.7);
        vis.widgets.active.weak_bg_fill = ORANGE.gamma_multiply(0.7);
        ctx.set_visuals(vis);
        ctx.all_styles_mut(|s| {
            // A tracker is monospace everywhere, not just in the grid.
            for (style, size) in [(TextStyle::Body, FONT), (TextStyle::Button, FONT), (TextStyle::Monospace, FONT),
                                  (TextStyle::Small, 11.0), (TextStyle::Heading, 18.0)] {
                s.text_styles.insert(style, FontId::monospace(size));
            }
            s.spacing.item_spacing = Vec2::new(6.0, 4.0);
            s.spacing.button_padding = Vec2::new(6.0, 2.0);
        });

        let sh = Arc::new(Mutex::new(Shared {
            st: Status::default(), song: Song::empty(120.0), slots: Vec::new(), kits: Vec::new(),
            kit_name: String::new(), midi_devs: Vec::new(), log: Vec::new(), connected: true, loaded: false,
        }));
        let (tx, rx) = channel::<Req>();
        {
            let sh = sh.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || worker(door, rx, sh, ctx));
        }
        App {
            sh, tx, track: 0, cur_step: 0, cur_slot: 0, accent: false, follow: true,
            bpm_local: 120.0, master_local: 2.0, song_path: "out/song.json".into(),
            wav_path: "out/song.wav".into(), pads_port: "COM5".into(), new_len: 16, midi_pick: 0,
            name_local: String::new(), name_track: None,
        }
    }

    fn send(&self, tool: &str, args: Value) {
        let _ = self.tx.send(Req { tool: tool.into(), args });
    }

    /// Mutate one cell locally and send the whole cell as `hit_set`: one verb, no drift.
    fn edit_cell(&self, track: usize, step: usize, f: impl FnOnce(&mut Vec<Hit>)) {
        let mut g = self.sh.lock().unwrap();
        let Some(t) = g.song.tracks.get_mut(track) else { return };
        t.normalise();
        if step >= t.len { return; }
        f(&mut t.cells[step]);
        let hits = t.cells[step].clone();
        drop(g);
        self.send("hit_set", json!({"track": track, "step": step, "hits": hits}));
    }

    fn edit_track(&self, track: usize, f: impl FnOnce(&mut Track), args: Value) {
        {
            let mut g = self.sh.lock().unwrap();
            let Some(t) = g.song.tracks.get_mut(track) else { return };
            f(t);
            t.normalise();
        }
        let mut a = args;
        a["track"] = json!(track);
        self.send("track_set", a);
    }

    /// Write a hit at the cursor cell, keeping any mods already on that slot there.
    fn write(&self, vel: u8) {
        let (track, step, slot) = (self.track, self.cur_step, self.cur_slot);
        self.edit_cell(track, step, |cell| {
            let mods = cell.iter().find(|h| h.slot == slot).map(|h| h.mods.clone()).unwrap_or_default();
            cell.retain(|h| h.slot != slot);
            cell.push(Hit { slot, vel, mods });
        });
    }

    fn clear(&self, track: usize, step: usize, slot: usize) {
        self.edit_cell(track, step, |cell| cell.retain(|h| h.slot != slot));
    }

    /// Toggle: a hit becomes empty, empty becomes x (or X with ACCENT lit).
    fn toggle(&self, track: usize, step: usize, slot: usize, present: bool) {
        if present { self.clear(track, step, slot); }
        else {
            let vel = if self.accent { 127 } else { 90 };
            self.edit_cell(track, step, |cell| cell.push(Hit { slot, vel, mods: Mods::default() }));
        }
    }

    fn mod_cursor(&self, f: impl FnOnce(&mut Mods)) {
        let slot = self.cur_slot;
        self.edit_cell(self.track, self.cur_step, |cell| {
            if let Some(h) = cell.iter_mut().find(|h| h.slot == slot) { f(&mut h.mods); }
        });
    }

    /// TAP: sound the slot now; while REC and playing, also write it at the nearest step,
    /// the way the pads record. Client-side quantise from the last status.
    fn tap(&self, slot: usize, snap: &Shared) {
        let hit = Hit { slot, vel: 100, mods: snap.st.armed.clone() };
        self.send("trigger", json!({"hit": hit}));
        if snap.st.recording && snap.st.playing {
            if let Some(t) = snap.song.tracks.get(self.track) {
                let step = quantise(snap.st.step, snap.st.phase, t.len);
                self.edit_cell(self.track, step, |cell| { cell.retain(|h| h.slot != slot); cell.push(hit); });
            }
        }
    }

    fn keys(&mut self, ctx: &egui::Context, snap: &Shared) {
        let len = snap.song.tracks.get(self.track).map_or(1, |t| t.len).max(1);
        let ntracks = snap.song.tracks.len().max(1);
        let events = ctx.input(|i| i.events.clone());
        for e in events {
            match e {
                egui::Event::Key { key, pressed: true, modifiers, .. } => match key {
                    Key::ArrowDown => self.cur_step = (self.cur_step + 1) % len,
                    Key::ArrowUp => self.cur_step = (self.cur_step + len - 1) % len,
                    Key::ArrowRight => self.cur_slot = (self.cur_slot + 1) % 9,
                    Key::ArrowLeft => self.cur_slot = (self.cur_slot + 8) % 9,
                    Key::PageDown => self.cur_step = (self.cur_step + 4).min(len - 1),
                    Key::PageUp => self.cur_step = self.cur_step.saturating_sub(4),
                    Key::Home => self.cur_step = 0,
                    Key::End => self.cur_step = len - 1,
                    Key::Tab => self.track = if modifiers.shift { (self.track + ntracks - 1) % ntracks } else { (self.track + 1) % ntracks },
                    Key::Delete | Key::Backspace => self.clear(self.track, self.cur_step, self.cur_slot),
                    Key::Enter => {
                        let present = snap.song.tracks.get(self.track).map_or(false, |t| t.cells.get(self.cur_step).map_or(false, |c| c.iter().any(|h| h.slot == self.cur_slot)));
                        self.toggle(self.track, self.cur_step, self.cur_slot, present);
                    }
                    Key::Space => self.send(if snap.st.playing { "stop" } else { "play" }, json!({})),
                    _ => {}
                },
                egui::Event::Text(t) => for ch in t.chars() {
                    match ch {
                        '1'..='9' => self.write(((ch as u8 - b'0') as u32 * 127 / 9) as u8),
                        '0' => self.clear(self.track, self.cur_step, self.cur_slot),
                        'a' => {
                            let slot = self.cur_slot;
                            self.edit_cell(self.track, self.cur_step, |cell| {
                                if let Some(h) = cell.iter_mut().find(|h| h.slot == slot) { h.vel = if h.vel >= 127 { 90 } else { 127 }; }
                            });
                        }
                        'r' => self.mod_cursor(|m| m.rev = if m.rev == Some(true) { None } else { Some(true) }),
                        '-' | '=' => {
                            let up = ch == '=';
                            self.mod_cursor(|m| {
                                let st = 12.0 * m.pitch.unwrap_or(1.0).max(1e-3).log2();
                                let st = (st + if up { 1.0 } else { -1.0 }).round().clamp(-36.0, 36.0);
                                m.pitch = if st == 0.0 { None } else { Some(2f32.powf(st / 12.0)) };
                            });
                        }
                        _ => if let Some(slot) = "zxcvbnm,.".find(ch) { self.tap(slot, snap); },
                    }
                },
                _ => {}
            }
        }
    }

    // ---- panels ------------------------------------------------------------------------
    fn top(&mut self, ui: &mut egui::Ui, snap: &Shared) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("TOASTED DRUMS").heading().color(ORANGE));
            ui.label(RichText::new("rhythm composer").small().color(NUM));
            ui.add_space(12.0);

            let playing = snap.st.playing;
            let start = egui::Button::new(RichText::new(if playing { "■ STOP" } else { "▶ START" }).strong())
                .fill(if playing { ORANGE.gamma_multiply(0.8) } else { GRID }).min_size(Vec2::new(90.0, 26.0));
            if ui.add(start).clicked() { self.send(if playing { "stop" } else { "play" }, json!({})); }

            let rec = egui::Button::new(RichText::new("● REC").strong().color(if snap.st.recording { Color32::WHITE } else { TEXT }))
                .fill(if snap.st.recording { LED_ON.gamma_multiply(0.8) } else { GRID }).min_size(Vec2::new(64.0, 26.0));
            if ui.add(rec).clicked() { self.send("record", json!({"on": !snap.st.recording})); }

            let acc = egui::Button::new(RichText::new("ACCENT").strong()).fill(if self.accent { STEP_COLORS[2].gamma_multiply(0.8) } else { GRID }).min_size(Vec2::new(70.0, 26.0));
            if ui.add(acc).on_hover_text("lit: new hits are written at full velocity (X)").clicked() { self.accent = !self.accent; }

            ui.add_space(12.0);
            ui.label(RichText::new("TEMPO").color(NUM));
            let r = ui.add_enabled(snap.loaded, egui::DragValue::new(&mut self.bpm_local).range(20.0..=300.0).speed(0.25).fixed_decimals(1));
            if r.changed() && snap.loaded { self.send("bpm", json!({"bpm": self.bpm_local})); }
            if !r.dragged() && !r.has_focus() && snap.loaded { self.bpm_local = snap.st.bpm; }

            ui.label(RichText::new("MASTER").color(NUM));
            let r = ui.add_enabled(snap.loaded, egui::DragValue::new(&mut self.master_local).range(0.0..=4.0).speed(0.02).fixed_decimals(2));
            if r.changed() && snap.loaded { self.send("master", json!({"gain": self.master_local})); }
            if !r.dragged() && !r.has_focus() && snap.loaded { self.master_local = snap.st.master; }

            ui.add_space(12.0);
            ui.label(RichText::new("KIT").color(NUM));
            let shown = if snap.kit_name.is_empty() { snap.st.kit.clone() } else { snap.kit_name.clone() };
            egui::ComboBox::from_id_salt("kit").selected_text(shown).width(140.0).show_ui(ui, |ui| {
                for k in &snap.kits {
                    let cur = k.ends_with(&snap.st.kit) || snap.st.kit.ends_with(k);
                    if ui.selectable_label(cur, k).clicked() && !cur { self.send("kit_load", json!({"path": k})); }
                }
            });

            ui.checkbox(&mut self.follow, "follow");

            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                let step = snap.st.step;
                let cyc = snap.song.cycle_steps().max(1);
                ui.label(RichText::new(format!("step {:>4}  cycle {:>3}/{cyc}", step, step % cyc as u64)).color(NUM));
                if !snap.connected { ui.label(RichText::new("DOOR LOST").strong().color(LED_ON)); }
            });
        });
    }

    fn patterns(&mut self, ui: &mut egui::Ui, snap: &Shared) {
        ui.label(RichText::new("PATTERNS").color(ORANGE).strong());
        ui.label(RichText::new("a track is a pattern with its own length; they all loop against one clock").small().color(NUM));
        ui.add_space(4.0);
        for (i, t) in snap.song.tracks.iter().enumerate() {
            let sel = i == self.track;
            let pos = if snap.st.playing { (snap.st.step % t.len.max(1) as u64) as usize } else { 0 };
            let hits: usize = t.cells.iter().map(Vec::len).sum();
            ui.horizontal(|ui| {
                let (r, resp) = ui.allocate_exact_size(Vec2::new(10.0, 10.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 4.0, if snap.st.playing && pos % 4 == 0 { LED_ON } else { LED_OFF });
                let _ = resp;
                // ◄ marks the pads context; M a muted track
                let label = format!("{i}{} {:<7} {:>2}/{:<3}{hits:>3}h{}", if snap.st.context == i { "◄" } else { " " },
                                    t.name.chars().take(7).collect::<String>(), pos, t.len, if t.mute { " M" } else { "" }) + &(if t.intensity > 0 { format!(" i{}", t.intensity) } else { String::new() });
                if ui.selectable_label(sel, RichText::new(label).monospace()).clicked() { self.track = i; }
            });
        }
        ui.add_space(6.0);
        if let Some(t) = snap.song.tracks.get(self.track) {
            let i = self.track;
            ui.separator();
            ui.horizontal(|ui| {
                if self.name_track != Some(i) { self.name_local = t.name.clone(); self.name_track = Some(i); }
                ui.label(RichText::new("name").color(NUM));
                let r = ui.add(egui::TextEdit::singleline(&mut self.name_local).desired_width(90.0));
                if r.lost_focus() && self.name_local != t.name {
                    let name = self.name_local.clone();
                    self.edit_track(i, |tr| tr.name = name.clone(), json!({"name": name}));
                } else if !r.has_focus() { self.name_local = t.name.clone(); }
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("len ").color(NUM));
                let mut len = t.len;
                if ui.add(egui::DragValue::new(&mut len).range(1..=128).speed(0.1)).changed() {
                    self.edit_track(i, |tr| tr.len = len, json!({"len": len}));
                }
                for (txt, l) in [("4/4", 16), ("3/4", 12), ("7/8", 14)] {
                    if ui.small_button(txt).clicked() { self.edit_track(i, |tr| tr.len = l, json!({"len": l})); }
                }
            });
            ui.horizontal(|ui| {
                let mut mute = t.mute;
                if ui.checkbox(&mut mute, "mute").changed() { self.edit_track(i, |tr| tr.mute = mute, json!({"mute": mute})); }
                let mut inten = t.intensity as i32;
                ui.label("layer");
                if ui.add(egui::DragValue::new(&mut inten).range(0..=3)).on_hover_text("0 always plays; 1-3 come in as the game's danger rises").changed() {
                    self.edit_track(i, |tr| tr.intensity = inten as u8, json!({"intensity": inten}));
                }
                if ui.small_button("pads here").on_hover_text("the plates play and record into this track").clicked() {
                    self.send("pads_context", json!({"track": i}));
                }
            });
            ui.horizontal(|ui| {
                if ui.small_button("clear").clicked() {
                    { let mut g = self.sh.lock().unwrap(); if let Some(tr) = g.song.tracks.get_mut(i) { for c in &mut tr.cells { c.clear(); } } }
                    self.send("track_clear", json!({"track": i}));
                }
                if snap.song.tracks.len() > 1 && ui.small_button("remove").clicked() {
                    { let mut g = self.sh.lock().unwrap(); if i < g.song.tracks.len() { g.song.tracks.remove(i); } }
                    self.send("track_remove", json!({"track": i}));
                    self.track = self.track.saturating_sub(1);
                }
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("pads").color(NUM));
                let mut pads = t.pads;
                let mut changed = false;
                for p in pads.iter_mut() { changed |= ui.add(egui::DragValue::new(p).range(0..=8)).changed(); }
                if changed { self.edit_track(i, |tr| tr.pads = pads, json!({"pads": pads})); }
            }).response.on_hover_text("which kit slot each of the four plates plays while this track is the pads context");
        }
        ui.separator();
        ui.horizontal(|ui| {
            ui.add(egui::DragValue::new(&mut self.new_len).range(1..=128));
            if ui.button("+ pattern").clicked() {
                let n = snap.song.tracks.len();
                self.send("track_add", json!({"name": format!("pat{n}"), "len": self.new_len}));
                self.track = n;
            }
        });

        ui.add_space(10.0);
        ui.label(RichText::new("FILE").color(ORANGE).strong());
        ui.add(egui::TextEdit::singleline(&mut self.song_path).desired_width(f32::INFINITY));
        ui.horizontal(|ui| {
            if ui.button("save").clicked() { self.send("song_save", json!({"path": self.song_path})); }
            if ui.button("load").clicked() { self.send("song_load", json!({"path": self.song_path})); }
        });
        ui.add(egui::TextEdit::singleline(&mut self.wav_path).desired_width(f32::INFINITY));
        if ui.button("render wav").on_hover_text("one full polymeter cycle, 16-bit").clicked() {
            self.send("render", json!({"path": self.wav_path}));
        }
    }

    fn instruments(&mut self, ui: &mut egui::Ui, snap: &Shared) {
        // The 808's instrument section, one block per kit slot, aligned with the grid columns.
        let track = snap.song.tracks.get(self.track);
        ui.horizontal(|ui| {
            ui.add_space(NUM_W);
            for slot in 0..9 {
                let info = snap.slots.get(slot);
                let color = info.map_or(DIM, |s| s.color);
                let label = info.map(|s| s.label.clone()).filter(|l| !l.is_empty()).unwrap_or_else(|| format!("slot{slot}"));
                let empty = info.map_or(true, |s| s.label.is_empty());
                let m = track.and_then(|t| t.cells.iter().flatten().find(|h| h.slot == slot)).map(|h| h.mods.clone()).unwrap_or_default();
                ui.allocate_ui_with_layout(Vec2::new(CELL_W - 4.0, 96.0), egui::Layout::top_down(Align::Min), |ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(2.0, 2.0);
                    let sel = slot == self.cur_slot;
                    let btn = egui::Button::new(RichText::new(label.to_uppercase()).strong().color(if empty { DIM } else { Color32::BLACK }))
                        .fill(if empty { GRID } else { color })
                        .stroke(if sel { Stroke::new(2.0, Color32::WHITE) } else { Stroke::NONE })
                        .min_size(Vec2::new(CELL_W - 6.0, 24.0));
                    let r = ui.add_enabled(!empty, btn).on_hover_text("click: tap (sounds now; records while REC + playing)\nselects the row of step buttons below");
                    if r.clicked() { self.cur_slot = slot; self.tap(slot, snap); }
                    if let Some(t) = track {
                        let i = self.track;
                        let last = t.len.saturating_sub(1);
                        let apply = |this: &App, mods: Mods| {
                            { let mut g = this.sh.lock().unwrap();
                              if let Some(tr) = g.song.tracks.get_mut(i) { for c in &mut tr.cells { for h in c.iter_mut().filter(|h| h.slot == slot) { h.mods = h.mods.over(&mods); } } } }
                            this.send("mods_apply", json!({"track": i, "from": 0, "to": last, "slot": slot, "mods": mods}));
                        };
                        let mut st = (12.0 * m.pitch.unwrap_or(1.0).max(1e-3).log2() as f64 * 10.0).round() / 10.0;
                        if knob(ui, "TUNE", &mut st, -36.0..=36.0, 0.1, 1, "st") {
                            apply(self, Mods { pitch: Some(2f32.powf(st as f32 / 12.0)), ..Default::default() });
                        }
                        let mut lv = m.gain.unwrap_or(1.0) as f64;
                        if knob(ui, "LEVEL", &mut lv, 0.0..=3.0, 0.01, 2, "") {
                            apply(self, Mods { gain: Some(lv as f32), ..Default::default() });
                        }
                        let mut dc = m.decay_ms.unwrap_or(0.0) as f64;
                        if knob(ui, "DECAY", &mut dc, 0.0..=4000.0, 2.0, 0, "ms") {
                            apply(self, Mods { decay_ms: Some(dc as f32), ..Default::default() });
                        }
                    }
                });
            }
        });
    }

    fn tracker(&mut self, ui: &mut egui::Ui, snap: &Shared) {
        let Some(track) = snap.song.tracks.get(self.track) else { return };
        let rows = track.len.max(1);
        let size = Vec2::new(NUM_W + 9.0 * CELL_W, rows as f32 * ROW_H);
        let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
        let p = ui.painter_at(rect);
        let font = FontId::monospace(FONT);
        let play_row = if snap.st.playing { Some((snap.st.step % rows as u64) as usize) } else { None };
        let row_rect = |r: usize| Rect::from_min_size(Pos2::new(rect.left(), rect.top() + r as f32 * ROW_H), Vec2::new(rect.width(), ROW_H));
        p.rect_filled(rect, 0.0, BG);
        for r in 0..rows {
            let rr = row_rect(r);
            if play_row == Some(r) { p.rect_filled(rr, 0.0, PLAYROW); }
            else if r % 4 == 0 { p.rect_filled(rr, 0.0, BEAT); }
            let numc = if r % 4 == 0 { STEP_COLORS[(r / 4) % 4] } else { NUM };
            p.text(Pos2::new(rr.left() + 4.0, rr.center().y), Align2::LEFT_CENTER, format!("{r:02X}"), font.clone(), numc);
            for c in 0..9 {
                let x = rect.left() + NUM_W + c as f32 * CELL_W;
                let cell = Rect::from_min_size(Pos2::new(x, rr.top()), Vec2::new(CELL_W, ROW_H));
                let hit = track.cells.get(r).and_then(|cell| cell.iter().find(|h| h.slot == c));
                let base = snap.slots.get(c).map_or(DIM, |s| s.color);
                let (txt, col) = match hit {
                    Some(h) => (format!("{:02X} {}", h.vel, flags(&h.mods)), if h.vel >= 127 { Color32::WHITE } else { base }),
                    None => ("-- ......".to_string(), DIM.gamma_multiply(0.8)),
                };
                if let Some(h) = hit { if h.vel >= 127 { p.rect_filled(cell.shrink2(Vec2::new(2.0, 1.0)), 2.0, base.gamma_multiply(0.35)); } }
                p.text(Pos2::new(x + 5.0, rr.center().y), Align2::LEFT_CENTER, txt, font.clone(), col);
                if (r, c) == (self.cur_step, self.cur_slot) {
                    p.rect_stroke(cell.shrink(0.5), 2.0, Stroke::new(1.5, ORANGE), StrokeKind::Inside);
                }
            }
        }
        for c in 0..=9 {
            let x = rect.left() + NUM_W + c as f32 * CELL_W - 1.0;
            p.line_segment([Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())], Stroke::new(1.0, GRID));
        }
        if let Some(pos) = resp.interact_pointer_pos() {
            let r = ((pos.y - rect.top()) / ROW_H).floor();
            let c = ((pos.x - rect.left() - NUM_W) / CELL_W).floor();
            if r >= 0.0 && (r as usize) < rows && (0.0..9.0).contains(&c) {
                let (r, c) = (r as usize, c as usize);
                if resp.clicked() || resp.secondary_clicked() { self.cur_step = r; self.cur_slot = c; }
                let present = track.cells.get(r).map_or(false, |cell| cell.iter().any(|h| h.slot == c));
                if resp.double_clicked() { self.toggle(self.track, r, c, present); }
                if resp.secondary_clicked() { self.clear(self.track, r, c); }
            }
        }
        if self.follow { if let Some(pr) = play_row { ui.scroll_to_rect(row_rect(pr), Some(Align::Center)); } }
    }

    fn steps(&mut self, ui: &mut egui::Ui, snap: &Shared) {
        // The 808's step buttons for the selected instrument. Rows of sixteen; a longer
        // pattern is "2nd part", a shorter one just stops early.
        let Some(track) = snap.song.tracks.get(self.track) else { return };
        let slot = self.cur_slot;
        let label = snap.slots.get(slot).map(|s| s.label.to_uppercase()).unwrap_or_default();
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("STEP  {label}")).color(ORANGE).strong());
            ui.label(RichText::new("click: hit / clear   shift+click: accent   right-click: clear   keys z x c v b n m , . tap").small().color(NUM));
        });
        let play = if snap.st.playing { Some((snap.st.step % track.len.max(1) as u64) as usize) } else { None };
        let shift = ui.input(|i| i.modifiers.shift);
        let w = 46.0;
        for row in 0..(track.len + 15) / 16 {
            ui.horizontal(|ui| {
                for k in 0..16 {
                    let step = row * 16 + k;
                    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 56.0), Sense::click());
                    if step >= track.len { continue; }
                    let hit = track.cells.get(step).and_then(|c| c.iter().find(|h| h.slot == slot));
                    let color = STEP_COLORS[k / 4];
                    let p = ui.painter();
                    // running light
                    p.circle_filled(Pos2::new(rect.center().x, rect.top() + 6.0), 4.0, if play == Some(step) { LED_ON } else { LED_OFF });
                    let body = Rect::from_min_max(Pos2::new(rect.left() + 2.0, rect.top() + 14.0), Pos2::new(rect.right() - 2.0, rect.bottom() - 2.0));
                    let fill = match hit { Some(_) => color, None => color.gamma_multiply(0.18) };
                    p.rect_filled(body, 3.0, fill);
                    if let Some(h) = hit {
                        if h.vel >= 127 { p.rect_stroke(body, 3.0, Stroke::new(2.0, Color32::WHITE), StrokeKind::Inside); }
                        p.text(Pos2::new(body.center().x, body.top() + 12.0), Align2::CENTER_CENTER, format!("{:02X}", h.vel), FontId::monospace(11.0), Color32::BLACK);
                    }
                    if resp.hovered() { p.rect_stroke(body, 3.0, Stroke::new(1.0, Color32::WHITE), StrokeKind::Outside); }
                    p.text(Pos2::new(body.center().x, body.bottom() - 9.0), Align2::CENTER_CENTER, format!("{}", step + 1), FontId::monospace(11.0),
                           if hit.is_some() { Color32::BLACK } else { color.gamma_multiply(0.7) });
                    if resp.clicked() {
                        self.cur_step = step; self.cur_slot = slot;
                        if shift || self.accent {
                            let mods = hit.map(|h| h.mods.clone()).unwrap_or_default();
                            self.edit_cell(self.track, step, |cell| { cell.retain(|h| h.slot != slot); cell.push(Hit { slot, vel: 127, mods }); });
                        } else { self.toggle(self.track, step, slot, hit.is_some()); }
                    }
                    if resp.secondary_clicked() { self.clear(self.track, step, slot); }
                }
            });
        }
    }

    fn side(&mut self, ui: &mut egui::Ui, snap: &Shared) {
        let Some(track) = snap.song.tracks.get(self.track) else { return };
        // ---- the hit under the cursor -------------------------------------------------
        let label = snap.slots.get(self.cur_slot).map(|s| s.label.clone()).unwrap_or_default();
        ui.label(RichText::new(format!("HIT  {:02X} {label}", self.cur_step)).color(ORANGE).strong());
        let hit = track.cells.get(self.cur_step).and_then(|c| c.iter().find(|h| h.slot == self.cur_slot)).cloned();
        match hit {
            None => {
                ui.label(RichText::new("empty -- Enter or 1-9 writes one").color(NUM));
            }
            Some(h) => {
                let mut vel = h.vel;
                ui.horizontal(|ui| {
                    ui.label(RichText::new("vel").color(NUM));
                    if ui.add(egui::Slider::new(&mut vel, 1..=127)).changed() {
                        let slot = self.cur_slot;
                        self.edit_cell(self.track, self.cur_step, |cell| if let Some(x) = cell.iter_mut().find(|x| x.slot == slot) { x.vel = vel; });
                    }
                });
                let mut m = h.mods.clone();
                if mods_editor(ui, &mut m, "hit") { self.mod_cursor(|mm| *mm = m.clone()); }
            }
        }
        ui.add_space(8.0);
        ui.separator();

        // ---- armed mods: ride on every recorded hit ----------------------------------
        ui.label(RichText::new("ARM  pads").color(ORANGE).strong()).on_hover_text("any modifier, any note: these ride on every hit recorded from the plates and on keyboard taps");
        let mut m = snap.st.armed.clone();
        if mods_editor(ui, &mut m, "arm") {
            self.sh.lock().unwrap().st.armed = m.clone();
            self.send("arm", json!({"mods": m, "target": "pads"}));
        }
        ui.label(RichText::new("ARM  keys").color(ORANGE).strong());
        let mut m = snap.st.keys_armed.clone();
        if mods_editor(ui, &mut m, "armk") {
            self.sh.lock().unwrap().st.keys_armed = m.clone();
            self.send("arm", json!({"mods": m, "target": "keys"}));
        }
        ui.add_space(8.0);
        ui.separator();

        // ---- pads -----------------------------------------------------------------------
        ui.label(RichText::new("PADS").color(ORANGE).strong());
        match &snap.st.pads {
            None => ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.pads_port).desired_width(50.0));
                if ui.button("open").on_hover_text("hands off the plates for 3 s while it calibrates").clicked() {
                    self.send("pads_open", json!({"port": self.pads_port}));
                }
            }).response,
            Some(p) => {
                let names: Vec<String> = p["names"].as_array().map(|a| a.iter().filter_map(|n| n.as_str().map(String::from)).collect()).unwrap_or_default();
                let learned = p["learned"].as_array().cloned().unwrap_or_default();
                for (i, n) in names.iter().enumerate() {
                    let l = learned.get(i).filter(|v| !v.is_null()).and_then(|v| v.as_array()).map(|v| format!("thr{:>4.0} ×{:>5.1}",
                        v.first().and_then(Value::as_f64).unwrap_or(0.0), v.get(1).and_then(Value::as_f64).unwrap_or(0.0)));
                    let slot = track.pads.get(i).copied().unwrap_or(0);
                    let sl = snap.slots.get(slot).map(|s| s.label.clone()).unwrap_or_default();
                    ui.label(RichText::new(format!("{n:<3}→{sl:<6} {}", l.unwrap_or_else(|| "unlearned".into()))).monospace());
                }
                ui.horizontal(|ui| {
                    if ui.button("learn").on_hover_text("three taps per pad; sets each pad's threshold and gain for this session").clicked() { self.send("pads_learn", json!({})); }
                    ui.label(RichText::new(format!("context: track {}", snap.st.context)).color(NUM));
                }).response
            }
        };
        ui.add_space(8.0);
        ui.separator();

        // ---- keys -------------------------------------------------------------------------
        ui.label(RichText::new("KEYS").color(ORANGE).strong());
        match &snap.st.midi {
            None => {
                ui.horizontal(|ui| {
                    let name = snap.midi_devs.iter().find(|(id, _)| *id == self.midi_pick).map(|(_, n)| n.clone()).unwrap_or_else(|| "no MIDI input".into());
                    egui::ComboBox::from_id_salt("midi").selected_text(name).width(150.0).show_ui(ui, |ui| {
                        for (id, n) in &snap.midi_devs { ui.selectable_value(&mut self.midi_pick, *id, n); }
                    });
                    if ui.button("open").clicked() { self.send("midi_open", json!({"id": self.midi_pick})); }
                    if ui.small_button("⟳").clicked() { self.send("midi_list", json!({})); }
                });
            }
            Some(m) => {
                ui.label(RichText::new(m["name"].as_str().unwrap_or("")).monospace());
                let mtrack = m["track"].as_u64().unwrap_or(0) as usize;
                let mode = m["mode"].as_str().unwrap_or("drums").to_string();
                let slot = m["slot"].as_u64().unwrap_or(0) as usize;
                let root = m["root"].as_u64().unwrap_or(36) as u8;
                let set = |this: &App, track: usize, mode: &str, slot: usize, root: u8|
                    this.send("midi_context", json!({"track": track, "mode": mode, "slot": slot, "root": root}));
                ui.horizontal(|ui| {
                    ui.label(RichText::new("into").color(NUM));
                    let mut t = mtrack;
                    if ui.add(egui::DragValue::new(&mut t).range(0..=snap.song.tracks.len().saturating_sub(1))).changed() { set(self, t, &mode, slot, root); }
                    if ui.selectable_label(mode == "drums", "drums").clicked() { set(self, mtrack, "drums", slot, root); }
                    if ui.selectable_label(mode == "chromatic", "chromatic").clicked() { set(self, mtrack, "chromatic", slot, root); }
                });
                if mode == "chromatic" {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("slot").color(NUM));
                        let mut s = slot;
                        if ui.add(egui::DragValue::new(&mut s).range(0..=8)).changed() { set(self, mtrack, &mode, s, root); }
                        ui.label(RichText::new("root").color(NUM));
                        let mut r = root;
                        if ui.add(egui::DragValue::new(&mut r).range(0..=127)).changed() { set(self, mtrack, &mode, slot, r); }
                    });
                }
            }
        }
    }

    fn log(&self, ui: &mut egui::Ui, snap: &Shared) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("LOG").color(NUM).small());
            let n = snap.log.len();
            let last = snap.log.iter().skip(n.saturating_sub(3)).cloned().collect::<Vec<_>>().join("   ·   ");
            ui.label(RichText::new(last).small().color(NUM));
        });
    }
}

/// One line of the instrument section: `TUNE  +0.0st`. Returns true when dragged.
fn knob(ui: &mut egui::Ui, label: &str, v: &mut f64, range: std::ops::RangeInclusive<f64>, speed: f64, decimals: usize, suffix: &str) -> bool {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).small().color(NUM));
        ui.add(egui::DragValue::new(v).range(range).speed(speed).fixed_decimals(decimals).suffix(suffix)).changed()
    }).inner
}

/// The mod flags of a hit as a fixed-width column: P D C R G L, or `.` where unset.
fn flags(m: &Mods) -> String {
    let f = |on: bool, c: char| if on { c } else { '.' };
    [f(m.pitch.is_some(), 'P'), f(m.drive.is_some(), 'D'), f(m.crush.is_some(), 'C'),
     f(m.rev == Some(true), 'R'), f(m.gain.is_some(), 'G'), f(m.decay_ms.is_some(), 'L')].iter().collect()
}

fn opt_row(ui: &mut egui::Ui, label: &str, v: &mut Option<f32>, default: f32, range: std::ops::RangeInclusive<f32>, speed: f64, decimals: usize, hint: &str) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let on = v.is_some();
        if ui.add_sized([58.0, 18.0], egui::Button::selectable(on, label)).on_hover_text(hint).clicked() {
            *v = if on { None } else { Some(default) };
            changed = true;
        }
        match v {
            Some(x) => changed |= ui.add(egui::DragValue::new(x).range(range).speed(speed).fixed_decimals(decimals)).changed(),
            None => { ui.label(RichText::new("default").color(DIM)); }
        }
    });
    changed
}

/// The per-hit mod editor. Pitch is shown in semitones because that is how one thinks
/// about it; it is stored as the ratio the engine wants.
fn mods_editor(ui: &mut egui::Ui, m: &mut Mods, salt: &str) -> bool {
    let mut changed = false;
    ui.push_id(salt, |ui| {
        ui.horizontal(|ui| {
            let on = m.pitch.is_some();
            if ui.add_sized([58.0, 18.0], egui::Button::selectable(on, "P pitch")).on_hover_text("semitones; the ratio 2^(st/12) is what is stored").clicked() {
                m.pitch = if on { None } else { Some(1.0) };
                changed = true;
            }
            match m.pitch.as_mut() {
                Some(p) => {
                    let mut st = (12.0 * p.max(1e-3).log2() * 10.0).round() / 10.0;
                    if ui.add(egui::DragValue::new(&mut st).range(-36.0..=36.0).speed(0.1).fixed_decimals(1).suffix("st")).changed() {
                        *p = 2f32.powf(st / 12.0);
                        changed = true;
                    }
                    ui.label(RichText::new(format!("×{p:.3}")).color(NUM).small());
                }
                None => { ui.label(RichText::new("default").color(DIM)); }
            }
        });
        changed |= opt_row(ui, "D drive", &mut m.drive, 2.0, 0.1..=12.0, 0.05, 2, "tanh saturation: 1 gentle, 4 wall, 10 fuzz");
        ui.horizontal(|ui| {
            let on = m.crush.is_some();
            if ui.add_sized([58.0, 18.0], egui::Button::selectable(on, "C crush")).on_hover_text("bit depth: 8 gritty, 4 destroyed").clicked() {
                m.crush = if on { None } else { Some(8) };
                changed = true;
            }
            match m.crush.as_mut() {
                Some(c) => changed |= ui.add(egui::DragValue::new(c).range(1..=15).suffix(" bit")).changed(),
                None => { ui.label(RichText::new("default").color(DIM)); }
            }
        });
        ui.horizontal(|ui| {
            let on = m.rev == Some(true);
            if ui.add_sized([58.0, 18.0], egui::Button::selectable(on, "R rev")).clicked() {
                m.rev = if on { None } else { Some(true) };
                changed = true;
            }
            ui.label(RichText::new(if on { "backwards" } else { "forwards" }).color(if on { TEXT } else { DIM }));
        });
        changed |= opt_row(ui, "G gain", &mut m.gain, 1.0, 0.0..=4.0, 0.01, 2, "linear level");
        changed |= opt_row(ui, "L decay", &mut m.decay_ms, 200.0, 1.0..=8000.0, 2.0, 0, "force the hit to fade to -60 dB by this many ms");
    });
    changed
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let snap = self.sh.lock().unwrap().clone();
        self.track = self.track.min(snap.song.tracks.len().saturating_sub(1));
        if let Some(t) = snap.song.tracks.get(self.track) { self.cur_step = self.cur_step.min(t.len.saturating_sub(1)); }
        if !ctx.egui_wants_keyboard_input() { self.keys(&ctx, &snap); }
        // keys may have edited; draw what they left behind
        let snap = self.sh.lock().unwrap().clone();

        let panel = |fill: Color32, m: i8| egui::Frame::NONE.fill(fill).inner_margin(Margin::same(m));
        egui::Panel::top("top").frame(panel(PANEL, 8)).show(ui, |ui| self.top(ui, &snap));
        egui::Panel::bottom("steps").frame(panel(PANEL, 8)).show(ui, |ui| {
            self.steps(ui, &snap);
            self.log(ui, &snap);
        });
        egui::Panel::left("patterns").resizable(false).exact_size(220.0).frame(panel(PANEL, 8)).show(ui, |ui| self.patterns(ui, &snap));
        egui::Panel::right("side").resizable(false).exact_size(270.0).frame(panel(PANEL, 8)).show(ui, |ui| self.side(ui, &snap));
        egui::CentralPanel::default().frame(panel(BG, 6)).show(ui, |ui| {
            self.instruments(ui, &snap);
            ui.add_space(4.0);
            egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| self.tracker(ui, &snap));
        });
        // The playhead moves between polls; keep the frame rate up while playing.
        ctx.request_repaint_after(Duration::from_millis(if snap.st.playing { 16 } else { 100 }));
    }
}

/// Attach to the door at `port`, or run the engine here and open the door ourselves.
pub fn run(port: u16, kit_path: &str, pads: Option<&str>) -> Result<(), String> {
    let (door, engine) = match Door::connect(port) {
        Ok(d) => { eprintln!("ui: attached to the door on 127.0.0.1:{port}"); (d, None) }
        Err(_) => {
            eprintln!("ui: nothing on 127.0.0.1:{port}; running the engine here ({kit_path})");
            let st = crate::mcp::open_studio(kit_path, pads)?;
            crate::mcp::serve_tcp(st.clone(), port)?;
            let d = Door::connect(port).map_err(|e| format!("ui: own door: {e}"))?;
            (d, Some(st))
        }
    };
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("ToastedDrums").with_inner_size([1360.0, 800.0]).with_min_inner_size([1000.0, 600.0]).with_maximized(true),
        ..Default::default()
    };
    let r = eframe::run_native("ToastedDrums", opts, Box::new(move |cc| Ok(Box::new(App::new(cc, door)))));
    drop(engine);
    r.map_err(|e| format!("ui: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_column_is_fixed_width_and_ordered() {
        assert_eq!(flags(&Mods::default()), "......");
        let m = Mods { pitch: Some(0.5), crush: Some(8), rev: Some(true), decay_ms: Some(100.0), ..Default::default() };
        assert_eq!(flags(&m), "P.CR.L");
        assert_eq!(flags(&Mods { rev: Some(false), ..Default::default() }), "......", "rev=false is not a flag");
    }

    #[test]
    fn status_parses_the_engine_shape_and_tolerates_nulls() {
        let v = json!({"playing": true, "step": 37, "phase": 0.25, "version": 9, "master": 2.0, "bpm": 128.0,
                       "kit": "kits/bigbeat.kit", "context": 1, "recording": false,
                       "armed": {"drive": 3.0}, "keys_armed": {}, "pads": null, "midi": {"id": 0}});
        let s = Status::from(&v);
        assert!(s.playing);
        assert_eq!((s.step, s.version, s.context), (37, 9, 1));
        assert_eq!(s.armed.drive, Some(3.0));
        assert!(s.pads.is_none());
        assert!(s.midi.is_some());
        let s = Status::from(&json!({}));
        assert_eq!(s.bpm, 0.0);
        assert!(s.armed == Mods::default());
    }

    #[test]
    fn kit_info_colours_become_slots() {
        let v = json!({"name": "K", "slots": [{"slot": 0, "label": "kick", "color": [255, 40, 0], "ms": 1.0},
                                              {"slot": 1, "label": null, "color": null, "ms": null}]});
        let (n, s) = slots_from(&v);
        assert_eq!(n, "K");
        assert_eq!(s[0].color, Color32::from_rgb(255, 40, 0));
        assert_eq!(s[1].label, "");
        assert_eq!(s[1].color, DIM);
    }
}
