"""Auto-Tune Modern Scale (ID 162) options and their ASSUMED pitch-class sets.

The labels and order come from reference/profiles/autotune-pro-38c42d0b-x64.json.
The interval sets are textbook definitions and are NOT verified against the plugin
(plan stage C). Diminished in particular is ambiguous (whole-half vs half-whole).
"""
import numpy as np

KEYS = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"]

# Profile order (index i -> normalized value i/14).
SCALES = {
    "Chromatic": tuple(range(12)),
    "Major": (0, 2, 4, 5, 7, 9, 11),
    "Minor": (0, 2, 3, 5, 7, 8, 10),
    "Harmonic Minor": (0, 2, 3, 5, 7, 8, 11),
    "Jazz Melodic Minor": (0, 2, 3, 5, 7, 9, 11),
    "Dorian": (0, 2, 3, 5, 7, 9, 10),
    "Phrygian": (0, 1, 3, 5, 7, 8, 10),
    "Lydian": (0, 2, 4, 6, 7, 9, 11),
    "Mixolydian": (0, 2, 4, 5, 7, 9, 10),
    "Locrian": (0, 1, 3, 5, 6, 8, 10),
    "Major Pentatonic": (0, 2, 4, 7, 9),
    "Minor Pentatonic": (0, 3, 5, 7, 10),
    "Blues": (0, 3, 5, 6, 7, 10),
    "Whole Tone": (0, 2, 4, 6, 8, 10),
    "Diminished": (0, 2, 3, 5, 6, 8, 9, 11),
}

# Combos with identical note sets sound identical; ties resolve in this order.
PREFERENCE = ["Major", "Minor"] + [s for s in SCALES if s not in ("Major", "Minor")]


def mask(key, scale):
    m = np.zeros(12, dtype=bool)
    for interval in SCALES[scale]:
        m[(key + interval) % 12] = True
    return m


def all_combos():
    """Every (key, scale) in preference order; Chromatic appears once (key C)."""
    combos = []
    for scale in PREFERENCE:
        for key in range(12):
            if scale == "Chromatic" and key:
                continue
            combos.append((key, scale))
    masks = np.array([mask(k, s) for k, s in combos])
    return combos, masks


def label(key, scale):
    return "Chromatic" if scale == "Chromatic" else f"{KEYS[key]} {scale}"


def transpose(key, scale, semitones):
    """Player transposition shifts the Key; the Scale is unchanged."""
    return (key + semitones) % 12, scale
