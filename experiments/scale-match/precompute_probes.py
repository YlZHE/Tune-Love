"""Precompute the per-song probe caches (S0 candidates, production evidence) in parallel.

run_holdout.py / replay_production.py read these caches when present, so this only
removes their sequential subprocess bottleneck. Files are published atomically.
Usage: precompute_probes.py --songs DIR --cache DIR --probe NEW_PROBE [--workers N]
"""
import argparse
import hashlib
import shutil
import sys
import tempfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

from replay_production import probe_rows
from run_pilot import s0_candidates


def key(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()[:16]


def work(path, cache_root, probe):
    cache = Path(cache_root) / key(path)
    if not (cache / "features.npz").exists():
        return f"skip {path.name}"
    for name, fn in (("s0-probe.json", lambda tmp: s0_candidates(path, tmp)),
                     ("production-evidence.json", lambda tmp: probe_rows(probe, path, tmp))):
        if (cache / name).exists():
            continue
        with tempfile.TemporaryDirectory(dir=cache) as tmp:
            try:
                fn(Path(tmp))
            except Exception as exc:
                return f"FAILED {path.name} {name} {exc}"
            if not (cache / name).exists():
                shutil.move(str(Path(tmp) / name), str(cache / name))
    return f"ok {path.name}"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--songs", required=True)
    parser.add_argument("--cache", required=True)
    parser.add_argument("--probe", required=True)
    parser.add_argument("--workers", type=int, default=6)
    args = parser.parse_args()
    sys.stdout.reconfigure(encoding="utf-8")
    songs = sorted(Path(args.songs).glob("*.mp3"))[::-1]  # from the end: the running evaluations start at the front
    with ThreadPoolExecutor(args.workers) as pool:
        for line in pool.map(lambda p: work(p, args.cache, Path(args.probe)), songs):
            print(line, flush=True)


if __name__ == "__main__":
    main()
