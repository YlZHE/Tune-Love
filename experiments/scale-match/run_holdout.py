"""Holdout evaluation on songs NOT used in the 5-song pilot, with parameters frozen beforehand.

Scale family is fixed to Major/Minor (+ Chromatic as the low-evidence state), per user decision.
First line: from LRC when present, otherwise the first moment the original vocal is active for
>= 1 s within a 2 s span (from the offline truth), lasting 5 s.
"""
import argparse
import json
import sys
from pathlib import Path

import numpy as np

import lrc
import matcher
from features import load_song
from run_pilot import (STEP, evaluate, harmony_at, matcher_combos, s0_candidates, s0_track,
                       stemgen_vocal_notes, vocal_truth)

PILOT_TITLES = {"___(Prod.AIRAVATA)", "少年深渊 (Prod.Kyon)", "怎？？Shinji prod：HARUHI", "泣", "神选"}
PILOT = {"32490ccc73e4af62", "f964e025e5538519", "4f6087517ed50731", "6294141b56ffe159", "c0153043250271d1"}
FROZEN = {
    "S1": matcher.Params(w_extra=1.0, tau=0.04, alpha=2.0, margin=0.03, gamma=4.0, halflife=0.0, family="diatonic"),
    "S3": matcher.Params(w_extra=1.0, tau=0.04, alpha=2.0, margin=0.01, gamma=4.0, halflife=0.0, family="diatonic"),
    "S3g": matcher.Params(w_extra=1.0, tau=0.04, alpha=2.0, margin=0.01, gamma=4.0, halflife=0.0, family="diatonic"),
    "S3b": matcher.Params(w_extra=1.0, tau=0.04, alpha=2.0, margin=0.01, gamma=4.0, halflife=0.0, family="diatonic"),
}
MIN_VOICED_SECONDS = 10.0


def first_vocal_line(f0_t, active_times):
    times = np.sort(active_times)
    for i, t in enumerate(times):
        j = np.searchsorted(times, t + 2.0)
        if (j - i) * 0.01 >= 1.0:
            return float(t), float(t) + 5.0
    return None


def oracle_diatonic(song):
    steps = int(np.ceil(float(song["data"]["duration"]) / STEP))
    allowed = np.flatnonzero(matcher.FAMILIES["diatonic"])
    scores = [(evaluate(song, np.full(steps + 1, i))["combined"] or 0.0, i) for i in allowed]
    return max(scores)[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--songs", required=True)
    parser.add_argument("--cache", required=True)
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    sys.stdout.reconfigure(encoding="utf-8")
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=False)
    report = {"schemaVersion": 1, "frozen": {k: v.__dict__ for k, v in FROZEN.items()}, "songs": [], "excluded": []}
    for path in sorted(Path(args.songs).glob("*.mp3")):
        try:
            digest, data = load_song(path, args.cache)
        except Exception as exc:  # e.g. a file that is not audio
            report["excluded"].append({"name": path.name, "reason": f"undecodable: {type(exc).__name__}"})
            continue
        if digest[:16] in PILOT or path.stem.rsplit(" - ", 1)[0] in PILOT_TITLES:
            report["excluded"].append({"name": path.name, "reason": "pilot song (or another file of it)"})
            continue
        times, notes, stats, free_times, free_sung = vocal_truth(data)
        if stats["voicedSeconds"] < MIN_VOICED_SECONDS:
            report["excluded"].append({"name": path.name, "reason": "little or no vocal", "truth": stats})
            continue
        lrc_path = path.with_suffix(".lrc")
        if lrc_path.exists():
            first, first_source = lrc.first_line(lrc.parse(lrc_path.read_text(encoding="utf-8-sig"))), "lrc"
        else:
            first, first_source = first_vocal_line(data["f0_t"], np.concatenate([times, free_times])), "vocal-activity"
        if first is None:
            report["excluded"].append({"name": path.name, "reason": "no sustained vocal onset", "truth": stats})
            continue
        cache_dir = Path(args.cache) / digest[:16]
        gen_times, gen_notes = stemgen_vocal_notes(cache_dir, "stemgenrt")
        bs_times, bs_notes = stemgen_vocal_notes(cache_dir, "bytesep")
        song = dict(bs_times=bs_times, bs_notes=bs_notes, name=path.name, sha256=digest, data=data, times=times, notes=notes, free_times=free_times,
                    free_sung=free_sung, harmony=harmony_at(data, free_times), stats=stats, first=first,
                    gen_times=gen_times, gen_notes=gen_notes)
        steps = int(np.ceil(float(data["duration"]) / STEP))
        res = {"S0": evaluate(song, s0_track(s0_candidates(path, cache_dir), steps)),
               "S4": evaluate(song, np.full(steps + 1, matcher.CHROMATIC)),
               "S5": evaluate(song, np.full(steps + 1, oracle_diatonic(song)))}
        for s, params in FROZEN.items():
            if (s == "S3g" and gen_times is None) or (s == "S3b" and bs_times is None):
                continue
            combos = matcher_combos(song, params, s)
            res[s] = evaluate(song, combos)
            res[s + "-cached"] = evaluate(song, np.full(steps + 1, combos[-1]))  # replay with last decision
        report["songs"].append({"name": path.name, "sha256": digest, "duration": round(float(data["duration"]), 1),
                                "firstLine": first, "firstLineSource": first_source, "truth": stats, "strategies": res})
        print("done", path.name, flush=True)
    (out / "holdout.json").write_text(json.dumps(report, ensure_ascii=False, indent=1), encoding="utf-8")
    summarize(report)


def summarize(report):
    names = ["S0", "S1", "S1-cached", "S3", "S3-cached", "S3g", "S3g-cached", "S3b", "S3b-cached", "S4", "S5"]
    print(f"\nsongs {len(report['songs'])}, excluded {len(report['excluded'])}")
    print("strategy      mispull  songs>5%mis  combined  firstLineComb  firstLineMis  switches")
    for n in names:
        rows = [s["strategies"][n] for s in report["songs"] if n in s["strategies"]]
        if not rows:
            continue
        mean = lambda xs: float(np.mean([x for x in xs if x is not None])) if any(x is not None for x in xs) else float("nan")
        mis = [r["whole"]["mispull"] for r in rows]
        print(f"{n:12s}  {mean(mis):7.3f}  {sum(m is not None and m > 0.05 for m in mis):>11d}  "
              f"{mean([r['combined'] for r in rows]):8.3f}  {mean([r['firstLineCombined'] for r in rows]):13.3f}  "
              f"{mean([r['firstLine']['mispull'] for r in rows]):12.3f}  {mean([r['switches'] for r in rows]):8.1f}")


if __name__ == "__main__":
    main()
