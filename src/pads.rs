//! pads — host driver for the `pads32` controller (classic ESP32 devkit, CP2102).
//!
//! IMPLEMENTS PROTOCOL.md v1. That document is the contract; this file and
//! `fw/pads32/pads32.ino` both conform to it. Change the contract first, then both halves.
//!
//! Win32 FFI, zero crates, house style.
//!
//! THE ONE THING THAT MATTERS HERE (PROTOCOL.md 1.1): DTR and RTS are wired to the devkit's
//! auto-reset transistors — RTS drives EN, DTR drives GPIO0. Letting Windows assert either
//! resets the ESP32, and DTR alone holds GPIO0 low, trapping it in the ROM bootloader
//! printing `waiting for download`. So the DCB leaves fDtrControl and fRtsControl at
//! DISABLE and uses no hardware flow control. Those are simply bits 4-5 and 12-13 of the
//! flags word staying zero.

use std::iter::once;

type Handle = isize;

const INVALID_HANDLE: Handle = -1;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const OPEN_EXISTING: u32 = 3;
const MAXDWORD: u32 = 0xFFFF_FFFF;
const PURGE_RXCLEAR: u32 = 0x0008;
const PURGE_TXCLEAR: u32 = 0x0004;
// EscapeCommFunction codes. Used to drive DTR/RTS explicitly rather than leaving their
// transitions to the OS — see `settle_lines`.
const CLRRTS: u32 = 4;
const CLRDTR: u32 = 6;

#[repr(C)]
struct Dcb {
    dcb_length: u32,
    baud_rate: u32,
    /// Packed bitfields. Bit 0 is fBinary (must be 1 on Windows); bits 4-5 fDtrControl and
    /// bits 12-13 fRtsControl stay 0 = DISABLE, which is what keeps the ESP32 running.
    flags: u32,
    w_reserved: u16,
    xon_lim: u16,
    xoff_lim: u16,
    byte_size: u8,
    parity: u8,
    stop_bits: u8,
    xon_char: i8,
    xoff_char: i8,
    error_char: i8,
    eof_char: i8,
    evt_char: i8,
    w_reserved1: u16,
}

#[repr(C)]
struct CommTimeouts {
    read_interval: u32,
    read_total_multiplier: u32,
    read_total_constant: u32,
    write_total_multiplier: u32,
    write_total_constant: u32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateFileW(name: *const u16, access: u32, share: u32, sa: *mut u8,
                   disposition: u32, flags: u32, template: Handle) -> Handle;
    fn CloseHandle(h: Handle) -> i32;
    fn ReadFile(h: Handle, buf: *mut u8, len: u32, read: *mut u32, ov: *mut u8) -> i32;
    fn WriteFile(h: Handle, buf: *const u8, len: u32, written: *mut u32, ov: *mut u8) -> i32;
    fn GetCommState(h: Handle, dcb: *mut Dcb) -> i32;
    fn SetCommState(h: Handle, dcb: *const Dcb) -> i32;
    fn SetCommTimeouts(h: Handle, t: *const CommTimeouts) -> i32;
    fn PurgeComm(h: Handle, flags: u32) -> i32;
    fn EscapeCommFunction(h: Handle, func: u32) -> i32;
    fn GetLastError() -> u32;
}

/// Force DTR and RTS deasserted, explicitly.
///
/// Setting DTR_CONTROL_DISABLE / RTS_CONTROL_DISABLE in the DCB describes the *steady*
/// state, but the OS still drives these lines around open and close, and on this board
/// RTS is EN and DTR is GPIO0. A badly-timed transition on close leaves GPIO0 low, so the
/// next reset lands in the ROM bootloader and the device sits there printing
/// `waiting for download` instead of running — observed in the field.
///
/// Driving both low deliberately, on open and again before close, keeps that transition
/// ours instead of the driver's. Best effort: failures here are not worth aborting over.
fn settle_lines(h: Handle) {
    unsafe {
        EscapeCommFunction(h, CLRDTR);
        EscapeCommFunction(h, CLRRTS);
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(once(0)).collect()
}

fn win_err(what: &str) -> String {
    format!("{what} failed (win32 error {})", unsafe { GetLastError() })
}

/// One strike. PROTOCOL.md 5.1.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    /// Index — the stable machine identifier. Key on this, not `name`.
    pub pad: u8,
    pub name: String,
    /// 1-127, already scaled by that pad's `gain`.
    pub vel: u8,
    /// Device milliseconds at onset. Monotonic since *its* boot, not wall clock, and wraps
    /// after ~49 days. Never compare across a device reboot.
    pub t_ms: u32,
    /// How far below baseline the strike reached, in COUNTS. Greater than that pad's
    /// `gain` means `vel` clipped at 127.
    pub depth: f32,
    /// Fall rate at the trigger, counts per ms. Reported only -- it gates nothing.
    pub slope: f32,
    pub base: u32,
    pub min: u32,
}

/// A reply line. PROTOCOL.md 4.
#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    pub ok: bool,
    /// The `#<seq>` tag echoed back, if the command carried one.
    pub seq: Option<u16>,
    /// Error code for failures (`badcmd`, `badarg`, `badpad`, `range`, `state`, `toolong`).
    pub code: Option<String>,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Msg {
    Hit(Hit),
    Reply(Reply),
    /// `p` — per-pad status from `get`.
    Pad { pad: u8, name: String, thresh: f32, slope: f32, gain: f32, base: u32 },
    /// `cfg` — config blob; round-trips verbatim as a preset.
    Config(String),
    /// `!` — human-readable notice. PROTOCOL.md 5.6: never parse these for control flow.
    Notice(String),
    /// `x` waveform, `w` window, `v` levels — diagnostics a host may ignore.
    Data(String),
    /// An unrecognised line type. PROTOCOL.md 5 requires hosts to ignore these rather than
    /// error, so the device can add message types without breaking us.
    Unknown(String),
}

pub struct Pads {
    h: Handle,
    /// Bytes received but not yet forming a complete line.
    pending: Vec<u8>,
    next_seq: u16,
}

impl Pads {
    /// Opens the port without disturbing DTR/RTS, so the device keeps running.
    pub fn open(port: &str, baud: u32) -> Result<Pads, String> {
        // The \\.\ form is required for COM10 and above, harmless below it.
        let path = wide(&format!(r"\\.\{port}"));
        let h = unsafe {
            CreateFileW(path.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0,
                        std::ptr::null_mut(), OPEN_EXISTING, 0, 0)
        };
        if h == INVALID_HANDLE {
            return Err(format!("{port}: {}", win_err("CreateFileW")));
        }

        let mut dcb: Dcb = unsafe { std::mem::zeroed() };
        dcb.dcb_length = std::mem::size_of::<Dcb>() as u32;
        if unsafe { GetCommState(h, &mut dcb) } == 0 {
            unsafe { CloseHandle(h) };
            return Err(format!("{port}: {}", win_err("GetCommState")));
        }
        dcb.baud_rate = baud;
        dcb.byte_size = 8;
        dcb.parity = 0;     // NOPARITY
        dcb.stop_bits = 0;  // ONESTOPBIT
        // fBinary only. Every other bit stays 0, disabling DTR, RTS, CTS/DSR flow control
        // and XON/XOFF. See the module header — this is not optional on this hardware.
        dcb.flags = 0x0000_0001;
        if unsafe { SetCommState(h, &dcb) } == 0 {
            unsafe { CloseHandle(h) };
            return Err(format!("{port}: {}", win_err("SetCommState")));
        }

        // Return whatever has arrived, immediately. The caller polls; a drum host runs a
        // loop and must never block waiting on a pad.
        let t = CommTimeouts {
            read_interval: MAXDWORD,
            read_total_multiplier: 0,
            read_total_constant: 0,
            write_total_multiplier: 0,
            write_total_constant: 500,
        };
        if unsafe { SetCommTimeouts(h, &t) } == 0 {
            unsafe { CloseHandle(h) };
            return Err(format!("{port}: {}", win_err("SetCommTimeouts")));
        }
        settle_lines(h);
        unsafe { PurgeComm(h, PURGE_RXCLEAR | PURGE_TXCLEAR) };

        Ok(Pads { h, pending: Vec::with_capacity(2048), next_seq: 1 })
    }

    fn write_line(&mut self, line: &str) -> Result<(), String> {
        let mut done = 0u32;
        let ok = unsafe {
            WriteFile(self.h, line.as_ptr(), line.len() as u32, &mut done, std::ptr::null_mut())
        };
        if ok == 0 { return Err(win_err("WriteFile")); }
        if done as usize != line.len() { return Err("short write to pads".into()); }
        Ok(())
    }

    /// Sends an untagged command.
    pub fn send(&mut self, cmd: &str) -> Result<(), String> {
        self.write_line(&format!("{cmd}\n"))
    }

    /// Sends a tagged command and returns the tag. PROTOCOL.md 3: unsolicited events
    /// interleave with replies, so tagging is the only way to match a reply to its command
    /// when more than one is in flight.
    pub fn send_tagged(&mut self, cmd: &str) -> Result<u16, String> {
        let seq = self.next_seq;
        self.next_seq = if self.next_seq == 65535 { 1 } else { self.next_seq + 1 };
        self.write_line(&format!("#{seq} {cmd}\n"))?;
        Ok(seq)
    }

    /// Introduce ourselves and open the session. Until `go` the device streams nothing —
    /// that is deliberate on its side, not a fault here.
    pub fn start(&mut self, who: &str) -> Result<(), String> {
        self.send(&format!("hello {who}"))?;
        self.send("go")
    }

    /// Non-blocking. Reads whatever has arrived and returns the complete lines it forms;
    /// a partial trailing line is held for next time.
    pub fn poll(&mut self) -> Result<Vec<Msg>, String> {
        let mut buf = [0u8; 4096];
        loop {
            let mut got = 0u32;
            let ok = unsafe {
                ReadFile(self.h, buf.as_mut_ptr(), buf.len() as u32, &mut got, std::ptr::null_mut())
            };
            if ok == 0 { return Err(win_err("ReadFile")); }
            if got == 0 { break; }
            self.pending.extend_from_slice(&buf[..got as usize]);
            if (got as usize) < buf.len() { break; }
        }

        let mut out = Vec::new();
        while let Some(nl) = self.pending.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = self.pending.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&raw[..nl]).trim_end().to_string();
            if !line.is_empty() { out.push(parse(&line)); }
        }
        Ok(out)
    }
}

impl Drop for Pads {
    fn drop(&mut self) {
        if self.h != INVALID_HANDLE {
            // Leave the control lines low deliberately. Without this, closing the port can
            // pulse GPIO0 and strand the device in the ROM bootloader.
            settle_lines(self.h);
            unsafe { CloseHandle(self.h) };
        }
    }
}

/// Splits an optional leading `#<seq>` off a reply body.
fn take_seq<'a>(f: &[&'a str]) -> (Option<u16>, usize) {
    match f.first().and_then(|t| t.strip_prefix('#')).and_then(|d| d.parse().ok()) {
        Some(s) => (Some(s), 1),
        None => (None, 0),
    }
}

fn parse(line: &str) -> Msg {
    let (tag, rest) = match line.split_once(' ') {
        Some((t, r)) => (t, r),
        None => (line, ""),
    };

    match tag {
        // h <pad> <name> <vel> <t_ms> <depth> <slope> <base> <min>   PROTOCOL.md 5.1
        "h" => {
            let f: Vec<&str> = rest.split_whitespace().collect();
            if f.len() >= 8 {
                if let (Ok(pad), Ok(vel), Ok(t_ms), Ok(depth), Ok(slope), Ok(base), Ok(min)) = (
                    f[0].parse::<u8>(), f[2].parse::<u8>(), f[3].parse::<u32>(),
                    f[4].parse::<f32>(), f[5].parse::<f32>(),
                    f[6].parse::<u32>(), f[7].parse::<u32>(),
                ) {
                    return Msg::Hit(Hit { pad, name: f[1].to_string(), vel, t_ms, depth, slope, base, min });
                }
            }
            // A malformed hit becomes Unknown rather than being dropped: losing a beat
            // silently is worse than surfacing a line we cannot read.
            Msg::Unknown(line.to_string())
        }

        // p <pad> <name> <thresh> <slope> <gain> <base>              PROTOCOL.md 5.5
        "p" => {
            let f: Vec<&str> = rest.split_whitespace().collect();
            if f.len() >= 6 {
                if let (Ok(pad), Ok(thresh), Ok(slope), Ok(gain), Ok(base)) = (
                    f[0].parse::<u8>(), f[2].parse::<f32>(), f[3].parse::<f32>(),
                    f[4].parse::<f32>(), f[5].parse::<u32>(),
                ) {
                    return Msg::Pad { pad, name: f[1].to_string(), thresh, slope, gain, base };
                }
            }
            Msg::Unknown(line.to_string())
        }

        "ok" | "err" => {
            let f: Vec<&str> = rest.split_whitespace().collect();
            let (seq, mut i) = take_seq(&f);
            let is_ok = tag == "ok";
            let code = if !is_ok && i < f.len() {
                let c = f[i].to_string(); i += 1; Some(c)
            } else { None };
            Msg::Reply(Reply { ok: is_ok, seq, code, text: f[i..].join(" ") })
        }

        "cfg" => Msg::Config(line.to_string()),
        "!"   => Msg::Notice(rest.to_string()),
        "x" | "w" | "v" => Msg::Data(line.to_string()),
        _ => Msg::Unknown(line.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hit() {
        match parse("h 2 P33 88 12400 51.0 9.10 502 246") {
            Msg::Hit(h) => {
                assert_eq!((h.pad, h.vel, h.t_ms, h.base, h.min), (2, 88, 12400, 502, 246));
                assert_eq!(h.name, "P33");
                assert!((h.depth - 51.0).abs() < 0.01);
                assert!((h.slope - 9.10).abs() < 0.01);
            }
            m => panic!("expected a hit, got {m:?}"),
        }
    }

    #[test]
    fn parses_pad_status() {
        match parse("p 0 P2 20.0 3.00 70.0 145") {
            Msg::Pad { pad, name, thresh, gain, base, .. } => {
                assert_eq!((pad, base), (0, 145));
                assert_eq!(name, "P2");
                assert!((thresh - 20.0).abs() < 0.01);
                assert!((gain - 70.0).abs() < 0.01);
            }
            m => panic!("expected pad status, got {m:?}"),
        }
    }

    #[test]
    fn parses_replies_with_and_without_tags() {
        match parse("ok go live") {
            Msg::Reply(r) => { assert!(r.ok); assert_eq!(r.seq, None); assert_eq!(r.text, "go live"); }
            m => panic!("got {m:?}"),
        }
        match parse("ok #7 set thresh P2 15.000") {
            Msg::Reply(r) => { assert!(r.ok); assert_eq!(r.seq, Some(7)); assert_eq!(r.text, "set thresh P2 15.000"); }
            m => panic!("got {m:?}"),
        }
        match parse("err #9 range thresh must be 1..90") {
            Msg::Reply(r) => {
                assert!(!r.ok);
                assert_eq!(r.seq, Some(9));
                assert_eq!(r.code.as_deref(), Some("range"));
                assert_eq!(r.text, "thresh must be 1..90");
            }
            m => panic!("got {m:?}"),
        }
        match parse("err badcmd unknown wibble") {
            Msg::Reply(r) => { assert!(!r.ok); assert_eq!(r.seq, None); assert_eq!(r.code.as_deref(), Some("badcmd")); }
            m => panic!("got {m:?}"),
        }
    }

    #[test]
    fn classifies_other_lines() {
        assert!(matches!(parse("! offer pads32 proto=1 pads=4"), Msg::Notice(_)));
        assert!(matches!(parse("cfg dur=10.0 p0=20.0,3.00,70.0"), Msg::Config(_)));
        assert!(matches!(parse("w 91 P2=143/145/147"), Msg::Data(_)));
        assert!(matches!(parse("v P2=145,144"), Msg::Data(_)));
        assert!(matches!(parse("x 0 P2 143 1 143,142,90"), Msg::Data(_)));
    }

    /// PROTOCOL.md 5: a host must ignore message types it does not know, so the device can
    /// add types without breaking us.
    #[test]
    fn unknown_types_are_ignorable_not_errors() {
        assert!(matches!(parse("q 1 2 3"), Msg::Unknown(_)));
        assert!(matches!(parse("h 1 P4"), Msg::Unknown(_)));
    }
}
