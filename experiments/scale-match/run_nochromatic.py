"""Re-score the 71 holdout songs after the user decision "never Chromatic".

Metric change: Chromatic (and "nothing written yet") = no correction. Strategy change:
the matcher only chooses Major/Minor and commits as soon as evidence exists.
These 71 songs were already seen (holdout-02), so this is a disclosed re-analysis,
not a fresh holdout. No parameter is tuned here; only the structural change is applied.
"""
import argparse
import dataclasses
import json
import sys
from pathlib import Path

import numpy as np

import lrc
import matcher
import run_pilot
from features import load_song
from metrics import success_table
from replay_production import production_combos
from run_holdout import FROZEN, MIN_VOICED_SECONDS, PILOT, PILOT_TITLES, first_vocal_line
from run_pilot import STEP, evaluate, harmony_at, s0_candidates, vocal_truth

run_pilot.TABLE = success_table(matcher.MASKS, chromatic_corrects=False)
MAJMIN = {k: dataclasses.replace(v, family="majmin") for k, v in FROZEN.items()}


def s0_track(cands, steps, votes_first=3, votes_replace=5):
    combo_of = {(c[0], c[1]): i for i, c in enumerate(matcher.COMBOS)}
    combos = np.full(steps + 1, matcher.CHROMATIC)
    confirmed, pending, streak = None, None, 0
    for k in range(1, steps + 1):
        cand = cands.get(k)
        key = (cand["pitchClass"], "Major" if cand["mode"] == "major" else "Minor") if cand else None
        if key is None or key == confirmed:
            pending, streak = None, 0
        else:
            streak = streak + 1 if key == pending else 1
            pending = key
            if streak >= (votes_replace if confirmed else votes_first):
                confirmed, pending, streak = key, None, 0
        combos[k] = combo_of[confirmed] if confirmed else matcher.CHROMATIC
    return combos


def production_majmin(rows, duration):
    saved = FROZEN["S1"]
    FROZEN["S1"] = MAJMIN["S1"]
    try:
        return production_combos(rows, duration)
    finally:
        FROZEN["S1"] = saved


def oracle_majmin(song):
    steps = int(np.ceil(float(song["data"]["duration"]) / STEP))
    allowed = np.flatnonzero(matcher.FAMILIES["majmin"])
    return max((evaluate(song, np.full(steps + 1, i))["combined"] or 0.0, i) for i in allowed)[1]


def cached(combos):
    """Second play: the first play's final choice is written from t = 0."""
    return np.full(len(combos), combos[-1])


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--songs", required=True)
    parser.add_argument("--cache", required=True)
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    sys.stdout.reconfigure(encoding="utf-8")
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=False)
    report = {"metric": "chromatic and unwritten = no correction", "majmin": {k: v.__dict__ for k, v in MAJMIN.items()},
              "songs": []}
    for path in sorted(Path(args.songs).glob("*.mp3")):
        try:
            digest, data = load_song(path, args.cache)
        except Exception:
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
        song = dict(name=path.name, data=data, times=times, notes=notes, free_times=free_times, free_sung=free_sung,
                    harmony=harmony_at(data, free_times), first=first)
        steps = int(np.ceil(float(data["duration"]) / STEP))
        cands = s0_candidates(path, cache_dir)
        rows = json.loads((cache_dir / "production-evidence.json").read_text(encoding="utf-8"))
        tracks = {
            "S0": s0_track(cands, steps),
            "S0-fast": s0_track(cands, steps, 1, 3),
            "PROD-old": production_combos(rows, float(data["duration"])),
            "PROD-majmin": production_majmin(rows, float(data["duration"])),
            "S5-majmin": np.full(steps + 1, oracle_majmin(song)),
        }
        for k in ("S0", "S0-fast", "PROD-majmin"):
            tracks[k + "-cached"] = cached(tracks[k])
        res = {k: evaluate(song, v) for k, v in tracks.items()}
        report["songs"].append({"name": path.name, "sha256": digest, "firstLine": first, "strategies": res})
        print("done", path.name, flush=True)
    (out / "nochromatic.json").write_text(json.dumps(report, ensure_ascii=False, indent=1), encoding="utf-8")
    names = ["S0", "S0-fast", "PROD-old", "PROD-majmin", "S0-cached", "S0-fast-cached", "PROD-majmin-cached", "S5-majmin"]
    mean = lambda xs: float(np.mean([x for x in xs if x is not None]))
    print(f"\nsongs {len(report['songs'])}")
    print("strategy             mispull  >5%  stableCorrect  combined  firstComb  firstCorrect  firstMis  switches  writtenAtFirst")
    for n in names:
        R = [s["strategies"][n] for s in report["songs"]]
        mis = [r["whole"]["mispull"] for r in R]
        print(f"{n:20s} {mean(mis):.3f} {sum(m > 0.05 for m in mis if m is not None):4d} "
              f"{mean([r['whole']['outputCorrect'] for r in R]):13.3f} {mean([r['combined'] for r in R]):9.3f} "
              f"{mean([r['firstLineCombined'] for r in R]):10.3f} {mean([r['firstLine']['outputCorrect'] for r in R]):13.3f} "
              f"{mean([r['firstLine']['mispull'] for r in R]):9.3f} {mean([r['switches'] for r in R]):9.1f} "
              f"{sum(r['narrowedBeforeFirstLine'] for r in R):>6d}/{len(R)}")


if __name__ == "__main__":
    main()
