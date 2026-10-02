"""Vocal-note truth and output-correctness metrics.

Snapping model (simplification of Auto-Tune): a sung pitch is pulled to the nearest
MIDI note whose pitch class is in the active set. "Correct" means it lands on the
original singer's note.
"""
import numpy as np

# Deviation (cents) -> weight. 0 = user sings correctly (failure there is a mis-pull).
DEVIATIONS = {0: 0.4, 30: 0.1, -30: 0.1, 60: 0.1, -60: 0.1, 90: 0.1, -90: 0.1}


def tuning_offset(midi):
    """Circular mean of deviation from the semitone grid, in semitones (-0.5..0.5)."""
    angle = 2 * np.pi * (midi - np.round(midi))
    return float(np.angle(np.exp(1j * angle).mean()) / (2 * np.pi)) if len(midi) else 0.0


def median_smooth(x, width=5):
    pad = width // 2
    padded = np.pad(x, pad, mode="edge")
    return np.median(np.lib.stride_tricks.sliding_window_view(padded, width), axis=1)


def note_frames(times, midi, voiced, hop, min_dur=0.12, max_std_cents=35.0):
    """Split voiced frames into constant-note segments.

    Returns (evaluable mask, note per frame as int MIDI, stats). Segments shorter than
    min_dur or with pitch spread above max_std_cents (glides, rap, breath) are
    unevaluable, never counted as correct or mis-pulled.
    """
    n = len(times)
    note = np.full(n, -1, dtype=int)
    evaluable = np.zeros(n, dtype=bool)
    offset = tuning_offset(midi[voiced])
    corrected = midi - offset
    smooth = median_smooth(np.where(voiced, corrected, np.nan) if n else corrected)
    rounded = np.where(voiced & np.isfinite(smooth), np.round(smooth), -1).astype(int)
    i = 0
    while i < n:
        if rounded[i] < 0:
            i += 1
            continue
        j = i
        while j + 1 < n and rounded[j + 1] == rounded[i]:
            j += 1
        seg = slice(i, j + 1)
        dur = (j - i + 1) * hop
        spread = float(np.std((corrected[seg] - rounded[i]) * 100.0))
        note[seg] = rounded[i]
        if dur >= min_dur and spread <= max_std_cents:
            evaluable[seg] = True
        i = j + 1
    stats = {
        "tuningOffsetCents": round(offset * 100, 1),
        "voicedSeconds": round(float(voiced.sum()) * hop, 2),
        "evaluableSeconds": round(float(evaluable.sum()) * hop, 2),
    }
    return evaluable, note, stats


def snap(sung, allowed_mask):
    """Nearest MIDI note with an allowed pitch class (sung is fractional MIDI)."""
    base = int(np.floor(sung))
    candidates = [m for m in range(base - 12, base + 13) if allowed_mask[m % 12]]
    return min(candidates, key=lambda m: (abs(m - sung), m))


def success_table(all_masks, chromatic_corrects=True):
    """[combo, true pitch class, deviation] -> lands on the true note.

    chromatic_corrects=False models the user's observation that Auto-Tune in
    Chromatic barely corrects a voice: an all-notes set (and the "nothing
    written yet" state, which evaluation also marks as Chromatic) then passes
    the sung pitch through, so only an in-tune note is "correct".
    """
    devs = list(DEVIATIONS)
    table = np.zeros((len(all_masks), 12, len(devs)), dtype=bool)
    for c, m in enumerate(all_masks):
        bypass = not chromatic_corrects and bool(np.all(m))
        for pc in range(12):
            n = 60 + pc
            for k, dev in enumerate(devs):
                table[c, pc, k] = (dev == 0) if bypass else snap(n + dev / 100.0, m) == n
    return table


def correct_rates(pcs, combo_idx, table):
    """Per-deviation fraction of frames landing on the true note.

    pcs: true pitch class per evaluated frame; combo_idx: active combo per frame.
    """
    if len(pcs) == 0:
        return {d: None for d in DEVIATIONS}
    hits = table[combo_idx, pcs, :].mean(axis=0)
    return {d: float(hits[k]) for k, d in enumerate(DEVIATIONS)}


def snap_pcs(sung, frame_masks):
    """Vectorized nearest-allowed snapping; returns the output pitch class per frame."""
    base = np.floor(sung).astype(int)
    cand = base[:, None] + np.arange(-6, 8)[None, :]
    allowed = frame_masks[np.arange(len(sung))[:, None], cand % 12]
    dist = np.where(allowed, np.abs(cand - sung[:, None]), np.inf)
    return cand[np.arange(len(sung)), np.argmin(dist, axis=1)] % 12


def consonance(sung, combo_idx, all_masks, harmony):
    """Unstable (speech-like/rap) frames: does the snapped note sound with the accompaniment?

    sung: tuning-corrected fractional MIDI of the original; harmony: [frame, 12] bool of
    pitch classes prominent in the accompaniment around that moment.
    """
    if len(sung) == 0:
        return {"frames": 0, "consonant": None, "byDeviation": {}}
    frame_masks = all_masks[combo_idx]
    rates = {}
    for dev in DEVIATIONS:
        pcs = snap_pcs(sung + dev / 100.0, frame_masks)
        rates[dev] = float(harmony[np.arange(len(sung)), pcs].mean())
    return {"frames": len(sung),
            "consonant": round(sum(DEVIATIONS[d] * r for d, r in rates.items()), 4),
            "byDeviation": {str(d): round(r, 4) for d, r in rates.items()}}


def combined(stable, free):
    """Frame-weighted mix of stable-note output correctness and unstable-frame consonance."""
    parts = [(stable["frames"], stable["outputCorrect"]), (free["frames"], free["consonant"])]
    parts = [(n, v) for n, v in parts if n and v is not None]
    total = sum(n for n, _ in parts)
    return round(sum(n * v for n, v in parts) / total, 4) if total else None


def summarize(pcs, combo_idx, table):
    rates = correct_rates(pcs, combo_idx, table)
    if rates[0] is None:
        return {"frames": 0, "mispull": None, "outputCorrect": None, "byDeviation": rates}
    weighted = sum(DEVIATIONS[d] * r for d, r in rates.items())
    return {
        "frames": len(pcs),
        "mispull": round(1.0 - rates[0], 4),
        "outputCorrect": round(weighted, 4),
        "byDeviation": {str(d): round(r, 4) for d, r in rates.items()},
    }
