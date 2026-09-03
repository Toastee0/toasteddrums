//! audio — real-time output via winmm `waveOut`. Win32 FFI, zero crates.
//!
//! WHY waveOut AND NOT WASAPI: WASAPI is the lower-latency path and is where this should
//! end up (see README "Next"), but it is COM — hand-rolled vtable dispatch, GUIDs and
//! reference counting, all without crates. waveOut is a flat C API, about a tenth of the
//! code, and gets a playable kit now. The cost is latency: roughly `BUFFERS * FRAMES /
//! rate`, so ~23 ms at the defaults here versus ~10 ms for WASAPI shared mode. A drummer
//! can feel the difference, so this is a first cut and not the final answer.
//!
//! Ownership rule that matters: once a WAVEHDR is handed to `waveOutWrite` the driver owns
//! it until it sets WHDR_DONE. The headers therefore live in a boxed slice that is never
//! resized or moved, and we only touch a buffer after seeing that flag.

const WAVE_MAPPER: u32 = 0xFFFF_FFFF;
const WAVE_FORMAT_PCM: u16 = 1;
const CALLBACK_NULL: u32 = 0;
const WHDR_DONE: u32 = 0x0000_0001;
const WHDR_PREPARED: u32 = 0x0000_0002;

/// Buffers queued to the driver. More is safer against underrun, worse for latency: the
/// queue depth IS the output latency. 3 x 128 @ 44.1k is ~8.7 ms, down from 4 x 256 (~23 ms),
/// which a player can feel. Going lower starts risking audible dropouts when the machine is
/// busy, and a dropout is far more annoying than a few ms.
const BUFFERS: usize = 3;
/// Frames per buffer. 128 @ 44.1k is 2.9 ms.
const FRAMES: usize = 128;

type HWaveOut = isize;

#[repr(C)]
struct WaveFormatEx {
    format_tag: u16,
    channels: u16,
    samples_per_sec: u32,
    avg_bytes_per_sec: u32,
    block_align: u16,
    bits_per_sample: u16,
    cb_size: u16,
}

#[repr(C)]
struct WaveHdr {
    data: *mut u8,
    buffer_length: u32,
    bytes_recorded: u32,
    user: usize,
    flags: u32,
    loops: u32,
    next: *mut WaveHdr,
    reserved: usize,
}

#[link(name = "winmm")]
unsafe extern "system" {
    fn waveOutOpen(out: *mut HWaveOut, device: u32, fmt: *const WaveFormatEx,
                   callback: usize, instance: usize, flags: u32) -> u32;
    fn waveOutPrepareHeader(h: HWaveOut, hdr: *mut WaveHdr, size: u32) -> u32;
    fn waveOutUnprepareHeader(h: HWaveOut, hdr: *mut WaveHdr, size: u32) -> u32;
    fn waveOutWrite(h: HWaveOut, hdr: *mut WaveHdr, size: u32) -> u32;
    fn waveOutReset(h: HWaveOut) -> u32;
    fn waveOutClose(h: HWaveOut) -> u32;
}

pub struct Out {
    h: HWaveOut,
    hdrs: Box<[WaveHdr]>,
    /// Backing store for the headers. Boxed so the pointers we hand the driver stay valid.
    bufs: Vec<Box<[i16]>>,
    pub rate: u32,
}

impl Out {
    pub fn open(rate: u32) -> Result<Out, String> {
        let fmt = WaveFormatEx {
            format_tag: WAVE_FORMAT_PCM,
            channels: 1,
            samples_per_sec: rate,
            avg_bytes_per_sec: rate * 2,
            block_align: 2,
            bits_per_sample: 16,
            cb_size: 0,
        };
        let mut h: HWaveOut = 0;
        let r = unsafe {
            waveOutOpen(&mut h, WAVE_MAPPER, &fmt, 0, 0, CALLBACK_NULL)
        };
        if r != 0 { return Err(format!("waveOutOpen failed (mmresult {r})")); }

        let mut bufs: Vec<Box<[i16]>> = Vec::with_capacity(BUFFERS);
        for _ in 0..BUFFERS { bufs.push(vec![0i16; FRAMES].into_boxed_slice()); }

        let mut hdrs: Vec<WaveHdr> = Vec::with_capacity(BUFFERS);
        for b in bufs.iter_mut() {
            hdrs.push(WaveHdr {
                data: b.as_mut_ptr() as *mut u8,
                buffer_length: (FRAMES * 2) as u32,
                bytes_recorded: 0,
                user: 0,
                // Start marked DONE so the first fill pass treats them all as free.
                flags: WHDR_DONE,
                loops: 0,
                next: std::ptr::null_mut(),
                reserved: 0,
            });
        }
        let mut hdrs = hdrs.into_boxed_slice();

        for hd in hdrs.iter_mut() {
            let r = unsafe { waveOutPrepareHeader(h, hd, std::mem::size_of::<WaveHdr>() as u32) };
            if r != 0 {
                unsafe { waveOutClose(h) };
                return Err(format!("waveOutPrepareHeader failed (mmresult {r})"));
            }
            hd.flags |= WHDR_DONE;
        }

        Ok(Out { h, hdrs, bufs, rate })
    }

    /// Frames the driver still holds — a rough queue depth for the caller's benefit.
    pub fn queued(&self) -> usize {
        self.hdrs.iter().filter(|hd| hd.flags & WHDR_DONE == 0).count() * FRAMES
    }

    /// Tops the queue back up, calling `fill` once per free buffer. `fill` writes exactly
    /// FRAMES mono samples in -1.0..1.0; anything outside is clipped rather than wrapped,
    /// because wrapping a drum transient sounds like a gunshot.
    pub fn pump<F: FnMut(&mut [f32])>(&mut self, mut fill: F) -> Result<(), String> {
        let mut scratch = [0f32; FRAMES];
        for i in 0..self.hdrs.len() {
            if self.hdrs[i].flags & WHDR_DONE == 0 { continue; }

            for s in scratch.iter_mut() { *s = 0.0; }
            fill(&mut scratch);

            let buf = &mut self.bufs[i];
            for (d, s) in buf.iter_mut().zip(scratch.iter()) {
                let v = if *s > 1.0 { 1.0 } else if *s < -1.0 { -1.0 } else { *s };
                *d = (v * 32767.0) as i16;
            }

            self.hdrs[i].flags &= !WHDR_DONE;
            let r = unsafe {
                waveOutWrite(self.h, &mut self.hdrs[i], std::mem::size_of::<WaveHdr>() as u32)
            };
            if r != 0 {
                self.hdrs[i].flags |= WHDR_DONE;
                return Err(format!("waveOutWrite failed (mmresult {r})"));
            }
        }
        Ok(())
    }
}

impl Drop for Out {
    fn drop(&mut self) {
        unsafe {
            waveOutReset(self.h);            // reclaim every queued buffer first
            for hd in self.hdrs.iter_mut() {
                if hd.flags & WHDR_PREPARED != 0 {
                    waveOutUnprepareHeader(self.h, hd, std::mem::size_of::<WaveHdr>() as u32);
                }
            }
            waveOutClose(self.h);
        }
    }
}
