//! Minimal RIFF/WAVE reader + writer. Handles 8/16/24-bit PCM and 32-bit float, any channel
//! count; decodes to interleaved f32 in -1..1. No resampling — the toykeyboards library is
//! uniformly 44100 Hz, and the engine runs at the sample rate of the first hit loaded.

#[derive(Clone, Debug)]
pub struct Wav {
    pub rate: u32,
    pub channels: u16,
    /// interleaved, -1..1
    pub data: Vec<f32>,
}

impl Wav {
    pub fn frames(&self) -> usize { self.data.len() / self.channels.max(1) as usize }

    /// Mono view: average of channels.
    pub fn mono(&self) -> Vec<f32> {
        let c = self.channels.max(1) as usize;
        self.data.chunks(c).map(|f| f.iter().sum::<f32>() / c as f32).collect()
    }

    pub fn parse(b: &[u8]) -> Result<Wav, String> {
        if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
            return Err("not a RIFF/WAVE file".into());
        }
        let mut pos = 12;
        let (mut fmt, mut bits, mut channels, mut rate) = (0u16, 0u16, 0u16, 0u32);
        let mut pcm: Option<&[u8]> = None;
        while pos + 8 <= b.len() {
            let id = &b[pos..pos + 4];
            let len = u32::from_le_bytes([b[pos + 4], b[pos + 5], b[pos + 6], b[pos + 7]]) as usize;
            let body = &b[pos + 8..(pos + 8 + len).min(b.len())];
            match id {
                b"fmt " if body.len() >= 16 => {
                    fmt = u16::from_le_bytes([body[0], body[1]]);
                    channels = u16::from_le_bytes([body[2], body[3]]);
                    rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
                    bits = u16::from_le_bytes([body[14], body[15]]);
                    if fmt == 0xFFFE && body.len() >= 26 { fmt = u16::from_le_bytes([body[24], body[25]]); }
                }
                b"data" => { pcm = Some(body); }
                _ => {}
            }
            pos += 8 + len + (len & 1);
        }
        let pcm = pcm.ok_or("no data chunk")?;
        if channels == 0 || rate == 0 { return Err("bad fmt chunk".into()); }
        let data: Vec<f32> = match (fmt, bits) {
            (1, 8) => pcm.iter().map(|&v| (v as f32 - 128.0) / 128.0).collect(),
            (1, 16) => pcm.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect(),
            (1, 24) => pcm.chunks_exact(3)
                .map(|c| (i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8) as f32 / 8388608.0).collect(),
            (1, 32) => pcm.chunks_exact(4).map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f32 / 2147483648.0).collect(),
            (3, 32) => pcm.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect(),
            _ => return Err(format!("unsupported format tag {fmt} / {bits} bit")),
        };
        Ok(Wav { rate, channels, data })
    }

    /// Serialize as 16-bit PCM.
    /// 32-bit float PCM (format 3): exact, for bakes the game must reproduce bit for bit.
    pub fn to_bytes_f32(&self) -> Vec<u8> {
        let n = self.data.len() * 4;
        let mut o = Vec::with_capacity(44 + n);
        o.extend_from_slice(b"RIFF");
        o.extend_from_slice(&((36 + n) as u32).to_le_bytes());
        o.extend_from_slice(b"WAVEfmt ");
        o.extend_from_slice(&16u32.to_le_bytes());
        o.extend_from_slice(&3u16.to_le_bytes());
        o.extend_from_slice(&self.channels.to_le_bytes());
        o.extend_from_slice(&self.rate.to_le_bytes());
        o.extend_from_slice(&(self.rate * self.channels as u32 * 4).to_le_bytes());
        o.extend_from_slice(&(self.channels * 4).to_le_bytes());
        o.extend_from_slice(&32u16.to_le_bytes());
        o.extend_from_slice(b"data");
        o.extend_from_slice(&(n as u32).to_le_bytes());
        for &s in &self.data { o.extend_from_slice(&s.to_le_bytes()); }
        o
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let n = self.data.len() * 2;
        let mut o = Vec::with_capacity(44 + n);
        o.extend_from_slice(b"RIFF");
        o.extend_from_slice(&((36 + n) as u32).to_le_bytes());
        o.extend_from_slice(b"WAVEfmt ");
        o.extend_from_slice(&16u32.to_le_bytes());
        o.extend_from_slice(&1u16.to_le_bytes());
        o.extend_from_slice(&self.channels.to_le_bytes());
        o.extend_from_slice(&self.rate.to_le_bytes());
        o.extend_from_slice(&(self.rate * self.channels as u32 * 2).to_le_bytes());
        o.extend_from_slice(&(self.channels * 2).to_le_bytes());
        o.extend_from_slice(&16u16.to_le_bytes());
        o.extend_from_slice(b"data");
        o.extend_from_slice(&(n as u32).to_le_bytes());
        for &s in &self.data {
            o.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
        }
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip() {
        let w = Wav { rate: 44100, channels: 2, data: vec![0.0, 0.5, -0.5, 1.0] };
        let p = Wav::parse(&w.to_bytes()).unwrap();
        assert_eq!(p.rate, 44100);
        assert_eq!(p.channels, 2);
        assert!((p.data[1] - 0.5).abs() < 1e-3);
        assert!((p.data[3] - 1.0).abs() < 1e-3);
    }
}
