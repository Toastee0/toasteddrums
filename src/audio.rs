//! audio — real-time output via cpal (WASAPI on Windows).
//!
//! WHY cpal REPLACED THE HAND-ROLLED waveOut: a pure sine through waveOut measured an
//! underrun every fourth buffer — 171 in 2 s — and the count was IDENTICAL whether we
//! spin-slept or blocked on the driver's completion event. That ruled out our pacing and
//! pointed at the layer below: waveOut on modern Windows is a shim over the WASAPI
//! shared-mode engine, which runs in ~10 ms periods and drains everything queued in one
//! gulp per period. A queue near one period is emptied entirely every period, so waveOut
//! cannot do sub-period latency at all; ~30-50 ms is its floor. The kit needs under 20.
//! cpal talks to WASAPI directly and can hit that.
//!
//! Supply chain, vetted before adding (the operator's condition): repository is
//! github.com/RustAudio/cpal, pinned =0.18.2, default features empty, every transitive
//! dependency resolves from the crates.io registry (no git/path sources), and what builds
//! on Windows is cpal + dasp_sample + Microsoft's `windows` bindings + dtolnay's proc-macro
//! trio. Do not loosen the pin casually.
//!
//! RATE: we open at the DEVICE'S native rate and channel count and never ask for the kit's.
//! That is how aire (github.com/Breijen/aire, a working cpal engine) does it, and it is the
//! difference between correct pitch and "playing slower than intended": asking WASAPI shared
//! mode for a rate its engine is not running at is either refused or run at the wrong
//! speed. Sources are pitch-corrected at mix time instead (see live.rs Mixer).
//!
//! MODEL: cpal owns the audio thread and calls `fill` for every buffer. The closure must
//! be fast, allocation-free and never block — no printing, no serial, no locks held long.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

/// The default output device's native sample rate. Callers build their fill closure
/// against this BEFORE opening the stream, since the closure needs to know it.
pub fn device_rate() -> Result<u32, String> {
    let device = cpal::default_host().default_output_device().ok_or("no default output device")?;
    let cfg = device.default_output_config().map_err(|e| format!("no default output config: {e}"))?;
    Ok(cfg.sample_rate())
}

pub struct Out {
    // Dropping the stream stops it. Kept for exactly that lifetime tie.
    _stream: cpal::Stream,
    pub rate: u32,
    pub channels: u16,
    /// Frames the device has asked us for so far. Lets a caller check the delivered rate
    /// against wall time — the direct test for "playing slower than intended".
    frames: Arc<AtomicU64>,
    /// Frames per callback as actually observed, so the latency we report is real rather
    /// than the number we asked for.
    per_call: Arc<AtomicU32>,
}

impl Out {
    /// Opens the default output device at ITS native config, delivering mono through
    /// `fill` and duplicating it to every hardware channel. `fill` receives a zeroed buffer
    /// of frames at `device_rate()` and sums into it, in -1.0..1.0.
    pub fn open<F>(mut fill: F) -> Result<Out, String>
    where
        F: FnMut(&mut [f32]) + Send + 'static,
    {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or("no default output device")?;
        // A fixed label: the device-name accessor changed across cpal releases and it is
        // only ever used in log and error strings, so it is not worth coupling to.
        let name = "default output";

        let supported = device.default_output_config()
            .map_err(|e| format!("{name}: no default output config: {e}"))?;
        let rate = supported.sample_rate();
        let channels = supported.channels();
        // Device-native everything, including its default buffer size. Requesting a fixed
        // small buffer is the other thing WASAPI shared mode is prone to refusing.
        let config: cpal::StreamConfig = supported.into();

        let frames = Arc::new(AtomicU64::new(0));
        let per_call = Arc::new(AtomicU32::new(0));
        let (counter, seen) = (frames.clone(), per_call.clone());
        let ch = channels as usize;
        let mut mono: Vec<f32> = Vec::new();

        let stream = device.build_output_stream(
            config,
            move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let n = out.len() / ch;
                if mono.len() < n { mono.resize(n, 0.0); }
                let m = &mut mono[..n];
                for s in m.iter_mut() { *s = 0.0; }
                fill(m);
                // Mono into every channel, with a hard clamp as the final safety net —
                // a wrapped drum transient sounds like a gunshot. The musical soft clip
                // happens upstream in the mixer.
                for (i, frame) in out.chunks_mut(ch).enumerate() {
                    let v = m[i].clamp(-1.0, 1.0);
                    for s in frame.iter_mut() { *s = v; }
                }
                counter.fetch_add(n as u64, Ordering::Relaxed);
                seen.store(n as u32, Ordering::Relaxed);
            },
            move |e| eprintln!("audio stream error: {e}"),
            None,
        ).map_err(|e| format!("{name}: build_output_stream: {e}"))?;

        stream.play().map_err(|e| format!("{name}: play: {e}"))?;
        eprintln!("audio: {name}, {rate} Hz native, {channels} ch");

        Ok(Out { _stream: stream, rate, channels, frames, per_call })
    }

    /// Total frames delivered to the device since open.
    pub fn frames_delivered(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    /// Our share of the output latency: one callback's worth of frames, as observed. The
    /// engine may add a period of its own on top in shared mode.
    pub fn latency_ms(&self) -> f32 {
        self.per_call.load(Ordering::Relaxed) as f32 * 1000.0 / self.rate as f32
    }
}
