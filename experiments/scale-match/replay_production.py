"""Replay the production evidence path (libkeyfinder stream -> per-hop chroma) through
the frozen matcher and score it exactly like the holdout, to check that the Rust
pipeline reproduces the offline S1 result.

Usage: replay_production.py --probe <key_window_probe.exe> --songs DIR --cache DIR --out DIR
"""
import argparse
import json
import subprocess
import sys
from pathlib import Path

import numpy as np

import lrc
import matcher
from features import decode, load_song
from run_holdout import FROZEN, MIN_VOICED_SECONDS, PILOT, PILOT_TITLES, first_vocal_line
from run_pilot import STEP, evaluate, harmony_at, vocal_truth

GAMMA = FROZEN["S1"].gamma


def probe_rows(probe, path, cache_dir):
    cached = cache_dir / "production-evidence.json"
    if cached.exists():
        return json.loads(cached.read_text(encoding="utf-8"))
    pcm = decode(path, 48_000, 2).T.reshape(-1).astype("<f4")
    seconds = min(900, len(pcm) // 96_000)
    out = subprocess.run([str(probe), "--mode", "incremental", "--end", str(seconds)],
                         input=pcm[:seconds * 96_000].tobytes(), capture_output=True, check=True)
    rows = [{"second": r["second"], "evidence": (r.get("stream") or {}).get("evidence")}
            for r in json.loads(out.stdout)["rows"]]
    cached.write_text(json.dumps(rows), encoding="utf-8")
    return rows


def production_combos(rows, duration):
    """Mirror scale_match.rs: per-step sharpen/normalize/weight, cumulative, hysteresis."""
    steps = int(np.ceil(duration / STEP))
    tracker = matcher.Tracker(FROZEN["S1"])
    evidence = np.zeros(12)
    combos = np.full(steps + 1, matcher.CHROMATIC)
    by_second = {r["second"]: r["evidence"] for r in rows if r["evidence"]}
    for k in range(1, steps + 1):
        ev = by_second.get(k)
        if ev and ev["seconds"] > 0 and ev["rms"] >= 1e-3:
            c = np.array(ev["chroma"], dtype=float)
            peak = c.max()
            if peak > 0:
                shaped = (c / peak) ** GAMMA
                evidence += shaped / shaped.sum() * ev["seconds"]
        combos[k] = tracker.update(evidence)
    return combos


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--probe", required=True)
    parser.add_argument("--songs", required=True)
    parser.add_argument("--cache", required=True)
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    sys.stdout.reconfigure(encoding="utf-8")
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=False)
    report = {"probe": str(args.probe), "songs": []}
    for path in sorted(Path(args.songs).glob("*.mp3")):
        try:
            digest, data = load_song(path, args.cache)
        except Exception as exc:  # undecodable file
            print("SKIP", path.name, exc, flush=True)
            continue
        if digest[:16] in PILOT or path.stem.rsplit(" - ", 1)[0] in PILOT_TITLES:
            continue
        times, notes, stats, free_times, free_sung = vocal_truth(data)
        if stats["voicedSeconds"] < MIN_VOICED_SECONDS:
            continue
        lrc_path = path.with_suffix(".lrc")
        first = (lrc.first_line(lrc.parse(lrc_path.read_text(encoding="utf-8-sig"))) if lrc_path.exists()
                 else first_vocal_line(data["f0_t"], np.concatenate([times, free_times])))
        if first is None:
            continue
        cache_dir = Path(args.cache) / digest[:16]
        rows = probe_rows(Path(args.probe), path, cache_dir)
        song = dict(name=path.name, data=data, times=times, notes=notes, free_times=free_times,
                    free_sung=free_sung, harmony=harmony_at(data, free_times), first=first)
        combos = production_combos(rows, float(data["duration"]))
        result = evaluate(song, combos)
        result["evidenceRows"] = sum(1 for r in rows if r["evidence"])
        report["songs"].append({"name": path.name, "sha256": digest, "strategies": {"S1-production": result}})
        print("done", path.name, result["combined"], result["finalCombo"], flush=True)
    (out / "replay.json").write_text(json.dumps(report, ensure_ascii=False, indent=1), encoding="utf-8")
    rows = [s["strategies"]["S1-production"] for s in report["songs"]]
    mean = lambda xs: float(np.mean([x for x in xs if x is not None]))
    print(f"\nS1-production over {len(rows)} songs: mispull {mean([r['whole']['mispull'] for r in rows]):.3f} "
          f"combined {mean([r['combined'] for r in rows]):.3f} firstLine {mean([r['firstLineCombined'] for r in rows]):.3f} "
          f"switches {mean([r['switches'] for r in rows]):.1f}")


if __name__ == "__main__":
    main()
