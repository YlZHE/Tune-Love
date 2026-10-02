"""Candidate evidence front end B: causal tuning -> log-frequency spectrum -> whitening -> pitch classes.

Written from the published principle of key-detection front ends (log-frequency spectrum,
per-song tuning estimate, spectral whitening, folding to 12 classes). No vendor code,
coefficients or tables are used; every constant below is our own choice, fixed in
preregistration.md (frontend-b-01) before any B result was seen.

Output has the same shape as features.chroma(): (times, chroma[n, 12], rms[n]), with C at
index 0 and each time stamp at the END of its frame (strictly causal).
"""
import numpy as np

from features import decode

RATE = 11_025
FRAME = 4096                 # ~0.372 s
HOP = 512                    # ~46.4 ms
SUB = 3                      # log-frequency bins per semitone
LOW_MIDI, HIGH_MIDI = 24, 105  # C1 .. A7
WHITEN_HALF = 18             # +-18 sub-bins = one octave window
SCALE_FLOOR = 1e-3           # relative to the frame maximum
FOLD_HALF = 1.5              # triangular folding kernel half-width, in sub-bins
# Revision b2: below this frequency one FFT bin (RATE/FRAME) is wider than a sub-bin (1/3 semitone),
# so sub-bin phases there reflect the FFT grid, not the tuning. Derived from resolution, not fitted.
TUNING_MIN_HZ = (RATE / FRAME) / (2.0 ** (1.0 / (12 * SUB)) - 1.0)   # ~138 Hz

N_BINS = (HIGH_MIDI - LOW_MIDI + 1) * SUB
# Pitch (fractional MIDI) of each sub-bin: offsets -1/3, 0, +1/3 around every semitone.
BIN_PITCH = LOW_MIDI + (np.arange(N_BINS) - (SUB // 2)) / SUB
BIN_OFFSET = ((np.arange(N_BINS) % SUB) - (SUB // 2)) / SUB


def midi_hz(m):
    return 440.0 * 2.0 ** ((np.asarray(m, dtype=float) - 69.0) / 12.0)


def logfreq_matrix():
    """[N_BINS, FRAME//2+1] triangular kernels mapping FFT magnitudes onto the log grid."""
    fft_hz = np.arange(FRAME // 2 + 1) * RATE / FRAME
    df = RATE / FRAME
    centres = midi_hz(BIN_PITCH)
    widths = np.maximum(centres * (2.0 ** (1.0 / (12 * SUB)) - 1.0), df)
    m = np.clip(1.0 - np.abs(fft_hz[None, :] - centres[:, None]) / widths[:, None], 0.0, None)
    return m / np.maximum(m.sum(axis=1, keepdims=True), 1e-12)


def smoothing_matrix():
    """[N_BINS, N_BINS] Hann-weighted moving average over +-WHITEN_HALF bins, renormalized at the edges."""
    w = np.hanning(2 * WHITEN_HALF + 3)[1:-1]                      # all taps > 0
    d = np.arange(N_BINS)[:, None] - np.arange(N_BINS)[None, :]
    m = np.where(np.abs(d) <= WHITEN_HALF, w[np.clip(d + WHITEN_HALF, 0, len(w) - 1)], 0.0)
    return m / m.sum(axis=1, keepdims=True)


def whiten(spec):
    """Positive standardized residual against a one-octave local background (per frame)."""
    s = smoothing_matrix()
    resid = spec - spec @ s.T
    scale = np.sqrt(np.maximum((resid ** 2) @ s.T, 0.0))
    floor = SCALE_FLOOR * spec.max(axis=-1, keepdims=True)
    scale = np.maximum(scale, np.maximum(floor, 1e-12))
    return np.maximum(resid, 0.0) / scale


def tuning_offsets(spec, min_hz=0.0):
    """Causal tuning estimate per frame (semitones, -0.5..0.5) from the cumulative raw spectrum.

    Only sub-bins centred at or above min_hz take part (b1: 0, b2: TUNING_MIN_HZ).
    """
    phase = np.exp(2j * np.pi * BIN_OFFSET) * (midi_hz(BIN_PITCH) >= min_hz)
    z = np.cumsum(spec @ phase, axis=0)
    delta = np.angle(z) / (2 * np.pi)
    return np.where(np.abs(z) > 0, delta, 0.0)


def fold(white, delta):
    """12 pitch classes from whitened sub-bins, centred on the tuning-shifted semitone grid."""
    n = white.shape[0]
    semis = np.arange(LOW_MIDI, HIGH_MIDI + 1)
    idx = np.arange(N_BINS)
    chroma = np.zeros((n, 12))
    for m in semis:
        centre = SUB * (m + delta - LOW_MIDI) + SUB // 2          # [n] fractional sub-bin
        k = np.clip(1.0 - np.abs(idx[None, :] - centre[:, None]) / FOLD_HALF, 0.0, None)
        chroma[:, m % 12] += (white * k).sum(axis=1)
    return chroma


def frames(mono):
    padded = np.concatenate([np.zeros(FRAME, dtype=np.float32), mono.astype(np.float32)])
    count = len(mono) // HOP
    starts = np.arange(1, count + 1) * HOP                         # frame j ends at padded[FRAME + jH]
    view = np.lib.stride_tricks.sliding_window_view(padded, FRAME)
    return view[starts], starts / RATE                             # end time of each frame


REVISIONS = {"b1": 0.0, "b2": TUNING_MIN_HZ}   # tuning estimate lower frequency limit


def analyze(mono, revision="b2"):
    """mono at RATE -> (times, chroma, rms, delta)."""
    x, times = frames(mono)
    rms = np.sqrt(np.mean(x.astype(np.float64) ** 2, axis=1))
    window = np.hanning(FRAME + 1)[:-1]                            # periodic Hann
    mag = np.abs(np.fft.rfft(x * window, axis=1))
    spec = mag @ logfreq_matrix().T
    delta = tuning_offsets(spec, REVISIONS[revision])
    chroma = fold(whiten(spec), delta)
    return times, chroma.astype(np.float32), rms.astype(np.float32), delta.astype(np.float32)


def load(path, cache_file, revision="b2"):
    """Front-end B features for one song, cached at cache_file (.npz; one cache folder per revision)."""
    if cache_file.exists():
        data = dict(np.load(cache_file))
        found = str(data.get("revision", "b1"))  # files written before revisions existed are b1
        if found != revision:
            raise ValueError(f"{cache_file} holds revision {found}, expected {revision}")
        return data
    mono = decode(path, RATE, 1)[0]
    times, chroma, rms, delta = analyze(mono, revision)
    data = dict(b_t=times, b_c=chroma, b_rms=rms, b_delta=delta, revision=np.array(revision))
    cache_file.parent.mkdir(parents=True, exist_ok=True)
    tmp = cache_file.with_name(cache_file.stem + ".tmp.npz")
    np.savez_compressed(tmp, **data)
    tmp.replace(cache_file)
    return data
