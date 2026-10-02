"""Mechanism R (recent_vocal.py): choose (w, h) on seen songs, then test on fresh songs.

  dev   seen songs = pilot + songs in --seen-report files; grid over w x h for fronts A+V and B2+V; write dev.json
  test  first --limit holdout songs by SHA-256 not in --seen-report files; compare X+V+R vs X+V; write test.json

Scoring: family majmin, Chromatic = no correction. Preregistration: preregistration.md, section frontend-b-04.
"""
import argparse
import dataclasses
import itertools
import json
import sys
from pathlib import Path

import numpy as np

import matcher
import recent_vocal
import run_pilot
from metrics import success_table
from run_frontend_b import compare, first_narrowing, kv, songs_in, summarize
from run_holdout import FROZEN
from scales import label

W_GRID = [0.5, 1.0, 2.0, 4.0]
H_GRID = [10.0, 20.0, 40.0]
FRONTS = {"A+V": "A", "B2+V": "b2"}


def setup(args):
    run_pilot.TABLE = success_table(matcher.MASKS, chromatic_corrects=False)
    b2 = json.loads(Path(args.b2_params).read_text(encoding="utf-8"))["chosen"]["B2+V"]["params"]
    params = {"A+V": dataclasses.replace(FROZEN["S3g"], family="majmin"),
              "B2+V": matcher.Params(**{**b2, "family": "majmin"})}
    seen = {s["sha256"] for f in args.seen_report for s in json.loads(Path(f).read_text(encoding="utf-8"))["songs"]}
    return params, seen


def combos(song, front, params, w, h):
    base, recent, _ = recent_vocal.streams(song["views"][FRONTS[front]], song["gen_times"], song["gen_notes"],
                                        float(song["data"]["duration"]), params[front], h)
    return recent_vocal.track(base, recent, params[front], w)


def score(song, c):
    r = run_pilot.evaluate(song, c)
    r["firstNarrowing"] = first_narrowing(c)
    return r


def oracle_mispull(song):
    steps = int(np.ceil(float(song["data"]["duration"]) / run_pilot.STEP))
    best = min((run_pilot.evaluate(song, np.full(steps + 1, i))["whole"]["mispull"], i)
               for i in np.flatnonzero(matcher.FAMILIES["majmin"]))
    return best[0], label(*matcher.COMBOS[best[1]])


def mean(xs):
    return float(np.mean([x for x in xs if x is not None]))


def dev(args):
    params, seen = setup(args)
    caches = kv(args.b_cache)
    songs = (songs_in(args.songs, args.cache, caches, "pilot")[0]
             + songs_in(args.songs, args.cache, caches, "holdout", only=seen)[0])
    report = {"songs": [s["name"] for s in songs], "grid": {"w": W_GRID, "h": H_GRID}, "fronts": {}}
    for front in FRONTS:
        base = [score(s, combos(s, front, params, 0.0, 10.0)) for s in songs]
        ref = {"mispull": mean([r["whole"]["mispull"] for r in base]), "combined": mean([r["combined"] for r in base]),
               "switches": mean([r["switches"] for r in base])}
        cells = []
        for w, h in itertools.product(W_GRID, H_GRID):
            res = [score(s, combos(s, front, params, w, h)) for s in songs]
            cell = {"w": w, "h": h, "mispull": mean([r["whole"]["mispull"] for r in res]),
                    "combined": mean([r["combined"] for r in res]), "switches": mean([r["switches"] for r in res]),
                    "songsOver5pct": sum(r["whole"]["mispull"] > 0.05 for r in res)}
            cell["eligible"] = cell["combined"] >= ref["combined"] and cell["switches"] <= ref["switches"] + 0.5
            cells.append(cell)
            print(front, cell, flush=True)
        eligible = [c for c in cells if c["eligible"]]
        # min mis-pull; ties: smaller w, then larger h (preregistered)
        chosen = min(eligible, key=lambda c: (round(c["mispull"], 6), c["w"], -c["h"])) if eligible else None
        report["fronts"][front] = {"baseline": ref, "cells": cells, "chosen": chosen}
        print(front, "baseline", ref, "chosen", chosen, flush=True)
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=False)
    (out / "dev.json").write_text(json.dumps(report, ensure_ascii=False, indent=1), encoding="utf-8")


def test(args):
    params, seen = setup(args)
    chosen = {f: v["chosen"] for f, v in json.loads(Path(args.dev).read_text(encoding="utf-8"))["fronts"].items()}
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=False)
    songs, excluded = songs_in(args.songs, args.cache, kv(args.b_cache), "holdout", args.limit, skip=seen)
    rows = []
    for s in songs:
        res = {}
        for front in FRONTS:
            res[front] = score(s, combos(s, front, params, 0.0, 10.0))
            if chosen[front]:
                res[front + "+R"] = score(s, combos(s, front, params, chosen[front]["w"], chosen[front]["h"]))
        oracle, oracle_key = oracle_mispull(s)
        rows.append({"name": s["name"], "sha256": s["sha256"], "duration": round(float(s["data"]["duration"]), 1),
                     "oracleMispull": oracle, "oracleKey": oracle_key, "strategies": res})
        print("done", s["name"], {k: (v["whole"]["mispull"], v["combined"]) for k, v in res.items()}, flush=True)
    labels = list(rows[0]["strategies"])
    pairs = [(f + "+R", f) for f in FRONTS if chosen[f]] + ([("B2+V+R", "A+V")] if chosen["B2+V"] else [])
    report = {"preregistration": "preregistration.md#frontend-b-04", "chosen": chosen, "excluded": excluded,
              "songs": rows, "summary": {l: summarize(rows, l) for l in labels},
              "avoidableMispull": {l: mean([r["strategies"][l]["whole"]["mispull"] - r["oracleMispull"] for r in rows])
                                   for l in labels},
              "oracleMispull": mean([r["oracleMispull"] for r in rows]),
              "comparisons": {f"{n} vs {b}": compare(rows, n, b) for n, b in pairs}}
    for n, b in pairs[:2]:
        report["verdict " + n] = report["comparisons"][f"{n} vs {b}"]["improved"]
    (out / "test.json").write_text(json.dumps(report, ensure_ascii=False, indent=1), encoding="utf-8")
    print(f"\nlabel     songs mispull  >5%  combined firstLine switches avoidable")
    for l, s in report["summary"].items():
        print(f"{l:9s} {s['songs']:5d} {s['mispull']:.4f} {s['songsMispullOver5pct']:4d}   {s['combined']:.4f}   "
              f"{s['firstLineCombined']:.4f}   {s['switches']:5.2f}  {report['avoidableMispull'][l]:.4f}")
    print("oracle mispull", round(report["oracleMispull"], 4))
    for name, c in report["comparisons"].items():
        print(f"{name}: improved={c['improved']} {c['checks']} combined W/L {c['pairedCombined']['wins']}/"
              f"{c['pairedCombined']['losses']} CI {c['pairedCombined']['ci95']}")
    print({k: v for k, v in report.items() if k.startswith("verdict")})


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("command", choices=["dev", "test"])
    parser.add_argument("--songs", required=True)
    parser.add_argument("--cache", required=True)
    parser.add_argument("--b-cache", action="append", required=True, help="REV=DIR (b2 required)")
    parser.add_argument("--b2-params", required=True)
    parser.add_argument("--seen-report", action="append", required=True)
    parser.add_argument("--dev", help="test: dev.json with the chosen (w, h)")
    parser.add_argument("--limit", type=int, default=10)
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    sys.stdout.reconfigure(encoding="utf-8")
    dev(args) if args.command == "dev" else test(args)


if __name__ == "__main__":
    main()
