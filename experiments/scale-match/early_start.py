"""Does starting analysis earlier help the first line? (seen data: the 71 holdout songs)

Production evidence rows from the probe with `--first 6` (current) and `--first 3`
(earliest; libkeyfinder needs ~3.75 s for its first hop). For each, simulate the
Major/Minor matcher with a first-write threshold of T seconds of evidence (nothing is
written before; "nothing written" = no correction), then the usual hysteresis.
"""
import argparse
import json
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import numpy as np

import matcher
import run_nochromatic as base  # sets the no-Chromatic metric table
from features import decode, load_song
from run_pilot import STEP, evaluate, harmony_at, vocal_truth

THRESHOLDS = (0.0, 2.0, 4.0)


def probe_rows(probe, path, cache_dir, first):
    cached = cache_dir / f"production-evidence-first{first}.json"
    if first == 6 and not cached.exists() and (cache_dir / "production-evidence.json").exists():
        cached = cache_dir / "production-evidence.json"
    if cached.exists():
        return json.loads(cached.read_text(encoding="utf-8"))
    pcm = decode(path, 48_000, 2).T.reshape(-1).astype("<f4")
    seconds = min(900, len(pcm) // 96_000)
    out = subprocess.run([str(probe), "--mode", "incremental", "--first", str(first), "--end", str(seconds)],
                         input=pcm[:seconds * 96_000].tobytes(), capture_output=True, check=True)
    rows = [{"second": r["second"], "evidence": (r.get("stream") or {}).get("evidence")}
            for r in json.loads(out.stdout)["rows"]]
    tmp = cached.with_suffix(".tmp")
    tmp.write_text(json.dumps(rows), encoding="utf-8")
    tmp.replace(cached)
    return rows


def simulate(rows, duration, threshold):
    params = base.MAJMIN["S1"]
    steps = int(np.ceil(duration / STEP))
    tracker = matcher.Tracker(params)
    evidence = np.zeros(12)
    combos = np.full(steps + 1, matcher.CHROMATIC)
    by_second = {r["second"]: r["evidence"] for r in rows if r["evidence"]}
    first_write = None
    for k in range(1, steps + 1):
        ev = by_second.get(k)
        if ev and ev["seconds"] > 0 and ev["rms"] >= 1e-3:
            c = np.array(ev["chroma"], dtype=float)
            if c.max() > 0:
                shaped = (c / c.max()) ** params.gamma
                evidence += shaped / shaped.sum() * ev["seconds"]
        if evidence.sum() >= max(threshold, 1e-9):
            tracker.update(evidence)
            if first_write is None and tracker.current != matcher.CHROMATIC:
                first_write = k
        combos[k] = tracker.current
    return combos, first_write


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--probe", required=True)
    parser.add_argument("--songs", required=True)
    parser.add_argument("--cache", required=True)
    parser.add_argument("--holdout", required=True)
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    sys.stdout.reconfigure(encoding="utf-8")
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=False)
    names = [s["name"] for s in json.loads(Path(args.holdout).read_text(encoding="utf-8"))["songs"]]
    firsts = {s["name"]: s["firstLine"] for s in json.loads(Path(args.holdout).read_text(encoding="utf-8"))["songs"]}

    def prepare(name):
        path = Path(args.songs) / name
        digest, _ = load_song(path, args.cache)
        cache_dir = Path(args.cache) / digest[:16]
        return name, {f: probe_rows(Path(args.probe), path, cache_dir, f) for f in (6, 3)}

    with ThreadPoolExecutor(8) as pool:
        rows_by_song = dict(pool.map(prepare, names))
    print("evidence ready", flush=True)

    report = {"thresholds": THRESHOLDS, "songs": []}
    for name in names:
        digest, data = load_song(Path(args.songs) / name, args.cache)
        times, notes, stats, free_times, free_sung = vocal_truth(data)
        song = dict(name=name, data=data, times=times, notes=notes, free_times=free_times, free_sung=free_sung,
                    harmony=harmony_at(data, free_times), first=firsts[name])
        res = {}
        for first in (6, 3):
            for t in THRESHOLDS:
                combos, first_write = simulate(rows_by_song[name][first], float(data["duration"]), t)
                r = evaluate(song, combos)
                r["firstWriteSecond"] = first_write
                res[f"start{first}-commit{t:g}"] = r
        first_evidence = {f: next((row["second"] for row in rows_by_song[name][f] if row["evidence"]), None) for f in (6, 3)}
        report["songs"].append({"name": name, "firstLine": firsts[name], "firstEvidenceSecond": first_evidence,
                                "strategies": res})
    (out / "early-start.json").write_text(json.dumps(report, ensure_ascii=False, indent=1), encoding="utf-8")

    mean = lambda xs: float(np.mean([x for x in xs if x is not None])) if xs else float("nan")
    songs = report["songs"]
    early = [s for s in songs if s["firstLine"][0] < 6.0]
    print(f"songs {len(songs)}; vocal onset < 6 s: {len(early)}")
    print("first evidence second (median): start6",
          np.median([s["firstEvidenceSecond"][6] for s in songs if s["firstEvidenceSecond"][6]]),
          "start3", np.median([s["firstEvidenceSecond"][3] for s in songs if s["firstEvidenceSecond"][3]]))
    print("strategy              writtenAtFirst  firstCorrect  firstMis  | onset<6s: firstCorrect firstMis | wholeCorrect  mispull  switches")
    for key in report["songs"][0]["strategies"]:
        R = [s["strategies"][key] for s in songs]
        E = [s["strategies"][key] for s in early]
        print(f"{key:20s} {sum(r['narrowedBeforeFirstLine'] for r in R):>6d}/{len(R)}  "
              f"{mean([r['firstLine']['outputCorrect'] for r in R]):12.3f} {mean([r['firstLine']['mispull'] for r in R]):9.3f}  | "
              f"{mean([r['firstLine']['outputCorrect'] for r in E]):21.3f} {mean([r['firstLine']['mispull'] for r in E]):8.3f} | "
              f"{mean([r['whole']['outputCorrect'] for r in R]):12.3f} {mean([r['whole']['mispull'] for r in R]):8.3f} "
              f"{mean([r['switches'] for r in R]):8.1f}")


if __name__ == "__main__":
    main()
