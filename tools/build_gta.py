"""build_gta.py -- writes songs/grand-theft-autumn.json.

Prototype, like tools/analyze_track.py: Python is the house prototyping language and the
artifact is the JSON, which the tracker UI and the MCP tools edit from there. This file
exists so the musical decisions are written down somewhere -- which roots, which register,
where the palm mute stops and the open bars start -- rather than being recoverable only by
reading 96 hits of JSON.

Eight bars of pop punk in D, 150 BPM, against a two-bar drum loop: 128 steps against 32 is
polymeter for free, and the cycle is their LCM.

    python tools/build_gta.py
    toasteddrums song kits/poppunk.kit songs/grand-theft-autumn.json out/gta.wav 128 1.0
"""
import json, os

# Grand-Theft-Autumn-shaped: key of D, 150 BPM. Eight bars of sixteenth steps.
#   bars 1-4  the interlude, one chord a bar: D  D  Bm  A
#   bars 5-8  the chorus shape, half a bar each: Bm G | D F#m | G A | D
# Roots sit in one octave, 46-73 Hz, which is where a bass is actually heavy.
ROOT = {"D": 38, "B": 35, "A": 33, "G": 31, "F#": 30, "E": 28}  # D2 B1 A1 G1 F#1 E1

BARS = [
    ["D", "D"], ["D", "D"], ["B", "B"], ["A", "A"],      # interlude
    ["B", "G"], ["D", "F#"], ["G", "A"], ["D", "D"],     # chorus
]
STEPS = len(BARS) * 16

def hit(slot, vel, **mods):
    return {"slot": slot, "vel": vel, "mods": {k: v for k, v in mods.items() if v is not None}}

# ---- drums: two bars, looping four times under the eight of bass -----------------------
# A 32-step track against a 128-step one is polymeter for free: the engine loops each track
# against the shared clock, so this is four passes of the drums per pass of the song.
drums = [[] for _ in range(32)]
for bar in range(2):
    b = bar * 16
    # kick on 1, the "and" of 2 and on 3; second bar takes a pickup on the last eighth
    for st, vel in [(0, 127), (6, 105), (8, 112)]:
        drums[b + st].append(hit(0, vel))
    if bar == 1:
        drums[b + 14].append(hit(0, 100))
    # backbeat
    for st in (4, 12):
        drums[b + st].append(hit(1, 122))
    # eighths on the hat, accented on the beat -- the density is the genre
    for st in range(0, 16, 2):
        drums[b + st].append(hit(3, 112 if st % 4 == 0 else 74))

# ---- crash: one per four-bar section ---------------------------------------------------
crash = [[] for _ in range(STEPS)]
crash[0].append(hit(8, 112))
crash[64].append(hit(8, 120))

# ---- bass ------------------------------------------------------------------------------
# Eighth-note roots, which is nearly all this genre's bass ever does. What makes it sound
# played is entirely in the articulation columns:
#   bars 1-4 are palm muted -- damp high, note cut short -- and bars 5-8 open up, which is
#   the arrangement doing the lifting rather than the notes;
#   a ghost note is the same note at low velocity and heavy damping: it reads as the right
#   hand, not as a pitch;
#   the last eighth of bar 4 SLIDES into the chorus instead of being picked again.
bass = [[] for _ in range(STEPS)]
VEL = [112, 84, 96, 84, 104, 84, 96, 88]  # one per eighth, downbeat hardest

for bar, halves in enumerate(BARS):
    muted = bar < 4
    for e in range(8):                      # eight eighths a bar
        step = bar * 16 + e * 2
        note = ROOT[halves[0 if e < 4 else 1]]
        # A palm mute is a short gate AND a lot of damping: the note stops well before the
        # next eighth, leaving the gap that makes eighths sound driven rather than merely
        # continuous. Open bars get no gate at all -- one string, so the next pluck takes it
        # over anyway, and the last note of the phrase is left to ring.
        bass[step].append(hit(2, VEL[e], note=note,
                              decay_ms=95.0 if muted else None,
                              damp=0.75 if muted else None))

# Ghost notes: the sixteenth before the downbeat of bars 3, 5 and 7. Low velocity and heavy
# damping, so they read as the right hand rather than as a pitch. Measured at about 18 dB
# under the downbeat -- quiet enough to be felt, loud enough to survive the drums.
for bar in (2, 4, 6):
    step = bar * 16 - 1
    bass[step] = [hit(2, 55, note=ROOT[BARS[bar - 1][1]], decay_ms=110.0, damp=0.85)]

# The turnaround: bar 4's last eighth leaves A and arrives on B without being picked.
# A slide needs something still ringing to slide FROM -- with nothing sounding the engine
# falls back to an ordinary pluck, silently, and the gesture just disappears. Bar 4 is palm
# muted, so its notes are gated at 95 ms and long dead by the next eighth; the note before
# the slide therefore has to come off the mute and be left open.
bass[60] = [hit(2, 100, note=ROOT["A"])]
bass[62] = [hit(2, 96, note=ROOT["B"], slide_ms=95.0)]

song = {
    "bpm": 150.0,
    "tracks": [
        {"name": "drums", "len": 32, "pads": [0, 3, 1, 8], "keys": {}, "cells": drums, "mute": False},
        {"name": "bass", "len": STEPS, "pads": [0, 3, 1, 2], "keys": {}, "cells": bass, "mute": False},
        {"name": "crash", "len": STEPS, "pads": [0, 3, 1, 8], "keys": {}, "cells": crash, "mute": False},
    ],
}

out = r'C:\projects\toasteddrums\songs'
os.makedirs(out, exist_ok=True)
path = os.path.join(out, 'grand-theft-autumn.json')
with open(path, 'w', encoding='utf-8') as f:
    json.dump(song, f, indent=1)
n = sum(len(c) for t in song["tracks"] for c in t["cells"])
print(f"{path}: {n} hits, {STEPS} steps, cycle = lcm(32,128) = 128")
