"""Reference stitching for the engine's WindowedSeparator; writes its Rust test fixture.

    stitch_fixture.py [--out devocal/engine/tests/fixtures/windowed_stitch.json]

Definition (plan Ruling 1, crossfade after kH per Task 3 ruling 3; stream_rt.py's hard cut
plus a 5 ms linear crossfade):
  - Segment k's window is the W input frames ending at kH+H+L (zeros before the stream start).
  - From the window take [W-L-H, W-L+xf), i.e. input frames [kH, kH+H+xf);
    accompaniment = window part - vocals part.
  - Over [kH, kH+xf) it crossfades linearly from segment k-1's extra tail (silence before
    segment 0): out = (1-r) prev + r cur, r_i = (i + 0.5) / xf. [kH+xf, kH+H) is segment k
    alone; its last xf frames are the tail for segment k+1.
  - A timed-out segment uses the dry input [kH, kH+H+xf) instead, with the same fades.
  - Streamed output frame t = accompaniment frame t - D, D = H + L + ceil(0.3 H); silence
    before.
Model: vocals = 0.5 x window. Input: xorshift32 stereo noise, sample = (x >> 8) / 2^24 - 0.5,
interleaved L, R (exact in f32; the Rust test regenerates it). The fixture stores the stream
output from frame D on as base64 little-endian f32. With this model every segment is 0.5 x, so
fresh-to-fresh fades cannot show the ramp; a second output with segment 1 timed out (dry, faded
against 0.5 x on both sides) pins the crossfade, stored over input frames [H, 2H + xf).
"""
import argparse
import base64
import json
import math
from pathlib import Path

import numpy as np

W, H, L, XF = 2000, 1250, 300, 220
SEED, BLOCKS, LATE = 0x2F6E2B1, 37, 1
D = H + L + math.ceil(0.3 * H)


def noise(seed, frames):
    x, out = seed, np.empty(frames * 2)
    for i in range(frames * 2):
        x ^= (x << 13) & 0xFFFFFFFF
        x ^= x >> 17
        x ^= (x << 5) & 0xFFFFFFFF
        out[i] = (x >> 8) / 16777216.0 - 0.5
    return out.reshape(frames, 2)


def stitch(x, w, h, la, xf, model, late=()):
    """Accompaniment aligned to the input (frame a <-> input frame a), shape of x."""
    n = len(x)
    xp = np.concatenate([np.zeros((w, 2)), x, np.zeros((w, 2))])  # xp[a + w] == x[a]
    acc = np.zeros((math.ceil(n / h) * h + xf, 2))  # whole segments plus the last tail
    r = ((np.arange(xf) + 0.5) / xf)[:, None]
    for k in range(math.ceil(n / h)):
        e = k * h + h + la
        win = xp[e:e + w]  # input frames [e - w, e)
        part = slice(w - la - h, w - la + xf)
        seg = xp[k * h + w:k * h + h + xf + w] if k in late else (win - model(win))[part]
        a = k * h
        acc[a:a + xf] = (1 - r) * acc[a:a + xf] + r * seg[:xf]
        acc[a + xf:a + h + xf] = seg[xf:]
    return acc[:n]


def main():
    ap = argparse.ArgumentParser()
    root = Path(__file__).resolve().parents[2]
    ap.add_argument("--out", type=Path, default=root / "devocal/engine/tests/fixtures/windowed_stitch.json")
    out_path = ap.parse_args().out

    frames = BLOCKS * 128
    x = noise(SEED, frames)
    acc = stitch(x, W, H, L, XF, lambda win: 0.5 * win)
    tail = acc[:frames - D].astype("<f4")  # stream output frames [D, frames)

    late = stitch(x, W, H, L, XF, lambda win: 0.5 * win, late={LATE})
    late_span = slice(LATE * H, LATE * H + H + XF)
    late_tail = late[late_span].astype("<f4")

    # Self-check: away from the fades a segment is exactly 0.5 x, or the dry input if it timed out.
    assert np.allclose(acc[H + XF:2 * H], 0.5 * x[H + XF:2 * H])
    assert np.array_equal(late[H + XF:2 * H], x[H + XF:2 * H])
    assert late_span.stop + D <= frames

    fx = dict(
        description="WindowedSeparator stitching reference (scripts/models/stitch_fixture.py); vocals = 0.5 x window",
        window_frames=W, hop_frames=H, lookahead_frames=L, crossfade_frames=XF, latency_frames=D,
        seed=SEED, input_frames=frames, input_head=[float(v) for v in x.reshape(-1)[:8]],
        output_start_frame=D,
        output_f32le_b64=base64.b64encode(tail.tobytes()).decode("ascii"),
        late_segment=LATE, late_output_start_frame=late_span.start + D,
        late_output_f32le_b64=base64.b64encode(late_tail.tobytes()).decode("ascii"),
    )
    out_path.write_text(json.dumps(fx, indent=1) + "\n", encoding="utf-8")
    print(f"wrote {out_path} ({out_path.stat().st_size} bytes, {tail.size} + {late_tail.size} samples)")


if __name__ == "__main__":
    main()
