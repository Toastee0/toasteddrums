//! Annunciator visualizer: a 3×3 panel frame from the engine's per-voice glow.
//! Slot v → tile (x = v % 3, y = v / 3), row-major, matching the firmware's 0x30 opcode.
//! Also the wire framing (COBS + CRC16-CCITT-FALSE), byte-identical to annunciator-rs/frame.rs.

use crate::kit::Kit;

pub const OP_PAINT: u8 = 0x30;
#[allow(dead_code)]
pub const OP_RELEASE: u8 = 0x31;

/// 27 bytes of RGB for a 3×3 panel. Idle tiles show the voice colour at floor brightness so
/// the kit layout is readable; a hit flashes to full colour and decays with the glow.
pub fn frame(kit: &Kit, glow: &[f32; 9], step: usize) -> [u8; 27] {
    let mut px = [0u8; 27];
    for v in 0..9 {
        let Some(voice) = &kit.voices[v] else { continue };
        let g = glow[v].clamp(0.0, 1.0);
        let floor = 0.03;
        let k = floor + (1.0 - floor) * g * g; // square = punchier attack
        for c in 0..3 { px[v * 3 + c] = (voice.color[c] as f32 * k) as u8; }
    }
    // the playhead: brighten the tile whose slot == step % 9 slightly (a running dot)
    let t = step % 9;
    for c in 0..3 { px[t * 3 + c] = px[t * 3 + c].saturating_add(8); }
    px
}

pub fn paint_packet(panel: u8, px: &[u8; 27]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(29);
    payload.push(OP_PAINT);
    payload.push(panel);
    payload.extend_from_slice(px);
    wrap(&payload)
}

#[allow(dead_code)]
pub fn release_packet(panel: u8) -> Vec<u8> { wrap(&[OP_RELEASE, panel]) }

pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

pub fn cobs_encode(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len() + src.len() / 254 + 2);
    let mut code_idx = 0;
    out.push(0);
    let mut code = 1u8;
    for &b in src {
        if b == 0 {
            out[code_idx] = code;
            code_idx = out.len();
            out.push(0);
            code = 1;
        } else {
            out.push(b);
            code += 1;
            if code == 0xFF {
                out[code_idx] = code;
                code_idx = out.len();
                out.push(0);
                code = 1;
            }
        }
    }
    out[code_idx] = code;
    out
}

/// COBS(payload ‖ crc16_be) ‖ 0x00
pub fn wrap(payload: &[u8]) -> Vec<u8> {
    let mut p = payload.to_vec();
    p.extend_from_slice(&crc16(payload).to_be_bytes());
    let mut out = cobs_encode(&p);
    out.push(0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crc_vector() { assert_eq!(crc16(b"123456789"), 0x29B1); }
    #[test]
    fn cobs_vector() { assert_eq!(cobs_encode(&[0x11, 0x22, 0x00, 0x33]), vec![0x03, 0x11, 0x22, 0x02, 0x33]); }
    #[test]
    fn packet_size() { assert!(paint_packet(1, &[0; 27]).len() <= 64); }
}
