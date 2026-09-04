"""analyze_track.py — measure what a reference track's drums actually do.

Prototype (Python is the house prototyping language; anything final is Rust). The point is
to turn "I want it to sound like Prodigy" into numbers a synth or a sample picker can use:
where the energy sits, the tempo, and for the strongest kick / snare / hat transients the
fundamental, the pitch drop, the decay, the brightness and the crest factor (a low crest
factor on a drum hit means it has been squashed or saturated — the big-beat signature).

Run:  uv run --with numpy --with scipy --with miniaudio python tools/analyze_track.py <file>
"""
import sys
import numpy as np
from scipy import signal
from scipy.io import wavfile
import miniaudio

# ---------------------------------------------------------------------------------------
def load_mono(path):
    d = miniaudio.decode_file(path, output_format=miniaudio.SampleFormat.FLOAT32, nchannels=1)
    x = np.frombuffer(d.samples, dtype=np.float32)
    return x, d.sample_rate

def db(x):
    return 20 * np.log10(np.maximum(x, 1e-12))

def band_energy(freqs, power, lo, hi):
    m = (freqs >= lo) & (freqs < hi)
    return power[m].sum()

BANDS = [
    ("sub      20-60",     20,    60),
    ("kick     60-120",    60,   120),
    ("lowmid  120-250",   120,   250),
    ("body    250-600",   250,   600),
    ("mid     600-2k",    600,  2000),
    ("crack   2k-5k",    2000,  5000),
    ("air     5k-10k",   5000, 10000),
    ("hats   10k-16k",  10000, 16000),
]

# ---------------------------------------------------------------------------------------
def long_term_spectrum(x, sr):
    f, p = signal.welch(x, sr, nperseg=8192)
    total = p.sum()
    print("\nLONG-TERM SPECTRUM  (share of total energy per band)")
    for name, lo, hi in BANDS:
        e = band_energy(f, p, lo, hi) / total
        bar = "#" * int(e * 120)
        print(f"  {name:<16} {e*100:5.1f}%  {bar}")

# ---------------------------------------------------------------------------------------
def onset_envelope(x, sr, hop=256, n=1024):
    """Spectral flux: how much each frame's spectrum rose vs the previous one."""
    win = np.hanning(n)
    frames = range(0, len(x) - n, hop)
    prev = None
    flux = []
    for i in frames:
        s = np.abs(np.fft.rfft(x[i:i + n] * win))
        if prev is None:
            flux.append(0.0)
        else:
            flux.append(np.maximum(s - prev, 0).sum())
        prev = s
    flux = np.array(flux)
    flux /= flux.max() + 1e-12
    return flux, hop

def tempo(flux, sr, hop):
    """Autocorrelate the onset envelope; the strongest lag in 90-180 BPM is the tempo.

    The range is deliberately narrow. With 60-200 the two-beat lag of a 134 BPM track wins
    and it reports 67 -- an octave error. 90-180 covers techno, big beat and breaks, which
    is the material this is for, and excludes the half-time alias."""
    f = flux - flux.mean()
    ac = np.correlate(f, f, mode="full")[len(f) - 1:]
    fps = sr / hop
    lo, hi = int(fps * 60 / 180), int(fps * 60 / 90)
    lag = lo + np.argmax(ac[lo:hi])
    return 60 * fps / lag

def pick_onsets(flux, hop, sr, count=400, min_gap_ms=60):
    """Strongest peaks of the flux, at least min_gap apart."""
    gap = int(min_gap_ms / 1000 * sr / hop)
    peaks, props = signal.find_peaks(flux, distance=gap, height=0.05)
    order = np.argsort(props["peak_heights"])[::-1][:count]
    return sorted(int(peaks[i]) * hop for i in order)

# ---------------------------------------------------------------------------------------
def hit_features(x, sr, start, length_ms=250):
    """Measure one transient starting at sample `start`."""
    seg = x[start:start + int(sr * length_ms / 1000)]
    if len(seg) < sr // 20:
        return None
    # attack window: first 30 ms decides what kind of drum this is
    a = seg[: int(sr * 0.03)]
    fa, pa = signal.periodogram(a, sr)
    low  = band_energy(fa, pa, 30, 150)
    mid  = band_energy(fa, pa, 150, 2500)
    high = band_energy(fa, pa, 2500, 16000)
    tot = low + mid + high + 1e-12
    kind = "kick" if low / tot > 0.55 else ("hat" if high / tot > 0.55 else "snare")

    # spectral centroid over the whole hit — the "brightness"
    f, p = signal.periodogram(seg, sr)
    centroid = (f * p).sum() / (p.sum() + 1e-12)

    # fundamental: strongest bin under 400 Hz in the first 40 ms
    fw = seg[: int(sr * 0.04)]
    ff, pf = signal.periodogram(fw, sr)
    m = (ff > 25) & (ff < 400)
    f0 = ff[m][np.argmax(pf[m])] if m.any() else 0.0

    # pitch trajectory: dominant low frequency per 10 ms frame over the first 120 ms —
    # electronic kicks sweep downward, and by how much is the character
    traj = []
    for k in range(0, 12):
        fr = seg[int(sr * k / 100): int(sr * (k + 1) / 100)]
        if len(fr) < 64:
            break
        tf, tp = signal.periodogram(fr * np.hanning(len(fr)), sr)
        mm = (tf > 25) & (tf < 400)
        traj.append(tf[mm][np.argmax(tp[mm])])
    sweep = (traj[0], traj[-1]) if len(traj) >= 2 else (f0, f0)

    # decay: time for the envelope to fall 20 dB below its peak
    env = np.abs(signal.hilbert(seg))
    env = signal.lfilter([1 / 64] * 64, [1], env)
    pk = env.max()
    below = np.where(env[np.argmax(env):] < pk * 10 ** (-20 / 20))[0]
    decay_ms = (below[0] / sr * 1000) if len(below) else length_ms

    # crest factor: peak / RMS. Clean acoustic hits sit ~12-18 dB; squashed, saturated
    # big-beat drums sit much lower. This is the "does it sound like a wall" number.
    rms = np.sqrt((seg ** 2).mean()) + 1e-12
    crest = db(np.abs(seg).max() / rms)

    return dict(kind=kind, f0=f0, sweep=sweep, centroid=centroid,
                decay_ms=decay_ms, crest=crest, level=db(np.abs(seg).max()), start=start)

# ---------------------------------------------------------------------------------------
def main(path, outdir):
    x, sr = load_mono(path)
    dur = len(x) / sr
    rms = np.sqrt((x ** 2).mean())
    print(f"{path}\n  {sr} Hz, {dur:.1f} s, peak {db(np.abs(x).max()):.1f} dBFS, "
          f"rms {db(rms):.1f} dBFS, crest {db(np.abs(x).max()/rms):.1f} dB (whole track)")

    long_term_spectrum(x, sr)

    flux, hop = onset_envelope(x, sr)
    print(f"\nTEMPO  ~{tempo(flux, sr, hop):.1f} BPM")

    onsets = pick_onsets(flux, hop, sr)
    feats = [f for f in (hit_features(x, sr, o) for o in onsets) if f]
    print(f"\nTRANSIENTS  {len(feats)} analysed  "
          f"(kick {sum(f['kind']=='kick' for f in feats)}, "
          f"snare {sum(f['kind']=='snare' for f in feats)}, "
          f"hat {sum(f['kind']=='hat' for f in feats)})")

    for kind in ("kick", "snare", "hat"):
        group = sorted((f for f in feats if f["kind"] == kind), key=lambda f: -f["level"])[:40]
        if not group:
            print(f"\n{kind.upper()}: none found"); continue
        med = lambda key: float(np.median([f[key] for f in group]))
        s0 = float(np.median([f["sweep"][0] for f in group]))
        s1 = float(np.median([f["sweep"][1] for f in group]))
        print(f"\n{kind.upper()}  (median of the {len(group)} loudest)")
        print(f"  fundamental      {med('f0'):6.1f} Hz")
        if kind == "kick":
            print(f"  pitch sweep      {s0:6.1f} -> {s1:6.1f} Hz over ~120 ms")
        print(f"  brightness       {med('centroid'):6.0f} Hz spectral centroid")
        print(f"  decay to -20 dB  {med('decay_ms'):6.0f} ms")
        print(f"  crest factor     {med('crest'):6.1f} dB   (lower = more squashed/saturated)")

        # keep the three loudest examples as WAVs so they can be listened to and compared
        for i, f in enumerate(group[:3]):
            seg = x[f["start"]: f["start"] + int(sr * 0.6)]
            wavfile.write(f"{outdir}/ref_{kind}_{i+1}.wav", sr, (seg * 32767).astype(np.int16))
    print(f"\nexample hits written to {outdir}/ref_<kind>_<n>.wav")

if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else ".")
