"""Precompute F0 for separated vocal stems (<stem>-vocals.npy -> <stem>-f0.npz) in every cache dir.

Usage: f0_stems.py CACHE_ROOT STEM [--wait N]   (set SCALE_MATCH_DEVICE=cuda to use the GPU)
With --wait N, keeps polling until N stems of that kind have F0 (for stems still being generated).
"""
import sys
import time
from pathlib import Path

import numpy as np

from features import SEP_RATE, vocal_f0

root, stem = Path(sys.argv[1]), sys.argv[2]
target = int(sys.argv[4]) if len(sys.argv) > 4 and sys.argv[3] == "--wait" else None
while True:
    for src in sorted(root.glob(f"*/{stem}-vocals.npy")):
        out = src.with_name(f"{stem}-f0.npz")
        if out.exists():
            continue
        vocals = np.load(src).astype(np.float32)
        f0_t, f0, period, f0_rms = vocal_f0(vocals.mean(0), SEP_RATE)
        tmp = src.with_name(f"{stem}-f0.tmp.npz")
        np.savez_compressed(tmp, f0_t=f0_t, f0=f0, period=period, f0_rms=f0_rms)
        tmp.replace(out)
        print("f0", src.parent.name, flush=True)
    done = len(list(root.glob(f"*/{stem}-f0.npz")))
    if target is None or done >= target:
        break
    time.sleep(20)
print("finished", stem, done, flush=True)
