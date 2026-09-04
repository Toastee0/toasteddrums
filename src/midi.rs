//! midi — input from a class-compliant USB-MIDI keyboard (the operator's Casio) via winmm.
//!
//! Win32 FFI, zero crates, mirroring `src/pads.rs`. Windows presents a USB-MIDI device as a
//! `midiIn` device; we open it with a callback and forward every note to a channel.
//!
//! THE THREADING RULE: the callback runs on a driver thread. It may only decode the message
//! and `try_send` it into a bounded channel — no locks, no allocation, and NEVER a `midiIn*`
//! call from inside the callback. `try_send` on a `sync_channel` neither blocks nor
//! allocates; if the host stops draining, notes are dropped (drums, not data).
//!
//! DROP ORDER matters: `midiInStop`, `midiInReset`, `midiInClose`, and only THEN free the
//! boxed sender the callback dereferences — the driver thread may still be inside the
//! callback until Close returns.
//!
//! Note-off is decoded but the engine ignores it: hits are one-shots, and sustain is a
//! `decay_ms` mod on the keys context. Casios send note-off as `0x9n` with velocity 0, so
//! `decode` treats that as off. Active Sensing (`0xFE`, every ~300 ms) and every other
//! system message is dropped.

use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError};

type Handle = isize;

const CALLBACK_FUNCTION: u32 = 0x0003_0000;
const MIM_DATA: u32 = 0x3C3;
const MMSYSERR_NOERROR: u32 = 0;

#[repr(C)]
struct MidiInCapsW {
    mid: u16,
    pid: u16,
    driver_version: u32,
    name: [u16; 32],
    support: u32,
}

#[link(name = "winmm")]
unsafe extern "system" {
    fn midiInGetNumDevs() -> u32;
    fn midiInGetDevCapsW(id: usize, caps: *mut MidiInCapsW, size: u32) -> u32;
    fn midiInOpen(h: *mut Handle, id: u32, callback: usize, instance: usize, flags: u32) -> u32;
    fn midiInStart(h: Handle) -> u32;
    fn midiInStop(h: Handle) -> u32;
    fn midiInReset(h: Handle) -> u32;
    fn midiInClose(h: Handle) -> u32;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MidiEvent {
    pub channel: u8,
    pub note: u8,
    /// 1-127 for note on; the raw velocity byte (usually 0) for note off
    pub vel: u8,
    pub on: bool,
}

/// Decodes a short MIDI message as packed by winmm into `dwParam1`:
/// `status | data1 << 8 | data2 << 16`. Only note on/off survive; everything else is None.
pub fn decode(p1: usize) -> Option<MidiEvent> {
    let status = (p1 & 0xFF) as u8;
    if status >= 0xF0 { return None; }                 // system messages incl. Active Sensing
    let data1 = ((p1 >> 8) & 0x7F) as u8;
    let data2 = ((p1 >> 16) & 0x7F) as u8;
    let channel = status & 0x0F;
    match status & 0xF0 {
        0x90 => Some(MidiEvent { channel, note: data1, vel: data2, on: data2 > 0 }),
        0x80 => Some(MidiEvent { channel, note: data1, vel: data2, on: false }),
        _ => None,
    }
}

unsafe extern "system" fn callback(_h: Handle, msg: u32, instance: usize, p1: usize, _p2: usize) {
    if msg != MIM_DATA { return; }
    if let Some(e) = decode(p1) {
        // The instance pointer is the boxed sender created in `open`; it outlives the
        // callback because `Drop` closes the device before freeing it.
        let tx = unsafe { &*(instance as *const SyncSender<MidiEvent>) };
        let _ = tx.try_send(e);
    }
}

pub struct Midi {
    h: Handle,
    rx: Receiver<MidiEvent>,
    /// Raw pointer to the boxed sender the driver holds, kept as usize so `Midi: Send`.
    tx_ptr: usize,
    pub name: String,
}

fn caps_name(caps: &MidiInCapsW) -> String {
    let end = caps.name.iter().position(|&c| c == 0).unwrap_or(caps.name.len());
    String::from_utf16_lossy(&caps.name[..end])
}

impl Midi {
    /// Every MIDI input the system knows about, as (id, name).
    pub fn list() -> Vec<(u32, String)> {
        let n = unsafe { midiInGetNumDevs() };
        (0..n).filter_map(|id| {
            let mut caps: MidiInCapsW = unsafe { std::mem::zeroed() };
            let r = unsafe { midiInGetDevCapsW(id as usize, &mut caps, std::mem::size_of::<MidiInCapsW>() as u32) };
            (r == MMSYSERR_NOERROR).then(|| (id, caps_name(&caps)))
        }).collect()
    }

    pub fn open(id: u32) -> Result<Midi, String> {
        let name = Midi::list().into_iter().find(|(i, _)| *i == id).map(|(_, n)| n)
            .ok_or(format!("no MIDI input device {id} (have {})", unsafe { midiInGetNumDevs() }))?;
        let (tx, rx) = sync_channel::<MidiEvent>(256);
        let tx_ptr = Box::into_raw(Box::new(tx)) as usize;
        let mut h: Handle = 0;
        let r = unsafe { midiInOpen(&mut h, id, callback as usize, tx_ptr, CALLBACK_FUNCTION) };
        if r != MMSYSERR_NOERROR {
            unsafe { drop(Box::from_raw(tx_ptr as *mut SyncSender<MidiEvent>)) };
            return Err(format!("midiInOpen({id}) failed (mmresult {r})"));
        }
        let r = unsafe { midiInStart(h) };
        if r != MMSYSERR_NOERROR {
            unsafe { midiInClose(h); drop(Box::from_raw(tx_ptr as *mut SyncSender<MidiEvent>)) };
            return Err(format!("midiInStart failed (mmresult {r})"));
        }
        Ok(Midi { h, rx, tx_ptr, name })
    }

    /// Non-blocking: everything received since the last call.
    pub fn poll(&mut self) -> Vec<MidiEvent> {
        let mut out = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(e) => out.push(e),
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        out
    }
}

impl Drop for Midi {
    fn drop(&mut self) {
        unsafe {
            midiInStop(self.h);
            midiInReset(self.h);
            midiInClose(self.h);
            // Only now: the driver has released the callback.
            drop(Box::from_raw(self.tx_ptr as *mut SyncSender<MidiEvent>));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(status: u8, d1: u8, d2: u8) -> usize {
        status as usize | (d1 as usize) << 8 | (d2 as usize) << 16
    }

    #[test]
    fn note_on_with_velocity() {
        assert_eq!(decode(pack(0x91, 36, 100)),
                   Some(MidiEvent { channel: 1, note: 36, vel: 100, on: true }));
    }

    #[test]
    fn note_on_velocity_zero_is_off() {
        // How Casios (and most keyboards) actually send note-off.
        assert_eq!(decode(pack(0x90, 60, 0)).map(|e| e.on), Some(false));
    }

    #[test]
    fn explicit_note_off() {
        assert_eq!(decode(pack(0x80, 60, 64)),
                   Some(MidiEvent { channel: 0, note: 60, vel: 64, on: false }));
    }

    #[test]
    fn system_and_other_messages_are_dropped() {
        assert_eq!(decode(pack(0xFE, 0, 0)), None, "active sensing");
        assert_eq!(decode(pack(0xB0, 7, 100)), None, "control change");
        assert_eq!(decode(pack(0xE0, 0, 64)), None, "pitch bend");
    }
}
