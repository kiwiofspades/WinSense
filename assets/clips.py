"""Synthesise the game clips for the mod port's `sound <name>` as wav files.

Run: python assets/clips.py <out folder>, then for each wav the recipe in docs/modding.md ("Voice and
sounds", Clips): ffmpeg to 200-byte Opus frames, then assets/ogg2frames.py to assets/<name>.bin.
Bell-like notes (a sine, a soft octave above, fast attack, exponential decay), like the dings.
"""
import math, random, struct, sys, wave

RATE = 48000

def note(hz, at, ring, level=0.5):
    """(start s, samples) of one bell note that rings for `ring` seconds."""
    n = int(ring * RATE)
    out = []
    for i in range(n):
        t = i / RATE
        env = min(1, t / 0.004) * math.exp(-5 * t / ring) * min(1, (n - i) / (0.01 * RATE))
        out.append(level * env * (math.sin(2 * math.pi * hz * t) + 0.3 * math.sin(4 * math.pi * hz * t)))
    return at, out

def hiss(at, secs, level=0.25):
    """Band-limited noise with a hard start: a radio keying up."""
    random.seed(7)
    n, out, lo, hi = int(secs * RATE), [], 0.0, 0.0
    for i in range(n):
        x = random.uniform(-1, 1)
        lo += 0.35 * (x - lo)       # low-pass near 3 kHz
        hi += 0.05 * (lo - hi)      # what is below about 400 Hz
        env = min(1, (n - i) / (0.02 * RATE))
        out.append(level * env * (lo - hi) * 2)
    return at, out

CLIPS = {
    "pickup": [note(1318.5, 0.0, 0.12), note(1975.5, 0.07, 0.25)],                       # E6 then B6
    "alert": [note(523.3, 0.0, 0.16, 0.6), note(415.3, 0.2, 0.3, 0.6)],                  # C5 then G#4, falling
    "radio": [note(2400, 0.0, 0.03, 0.6), hiss(0.01, 0.16), note(1800, 0.18, 0.03, 0.5)],  # click, hiss, click
    "levelup": [note(1046.5, 0.0, 0.14), note(1318.5, 0.08, 0.14), note(1568.0, 0.16, 0.14), note(2093.0, 0.24, 0.32)],  # C6 E6 G6 C7
}

for name, parts in CLIPS.items():
    total = max(int(at * RATE) + len(s) for at, s in parts)
    mix = [0.0] * total
    for at, s in parts:
        for i, v in enumerate(s):
            mix[int(at * RATE) + i] += v
    with wave.open(f"{sys.argv[1]}/{name}.wav", "wb") as w:
        w.setnchannels(1); w.setsampwidth(2); w.setframerate(RATE)
        w.writeframes(b"".join(struct.pack("<h", int(max(-1, min(1, v)) * 32000)) for v in mix))
    print(name, f"{total / RATE:.2f} s, peak {max(abs(v) for v in mix):.2f}")
