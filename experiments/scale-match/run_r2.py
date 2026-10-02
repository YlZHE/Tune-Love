"""Mechanism R2 = R + minimum dwell + accompaniment (whole-song mix) compatibility gate.
With --onset 5 --onset 10 (R3, section frontend-b-06) the grid also covers the onset delay N.

  dev   seen songs = pilot + songs in --seen-report files; grid w x h x D x tol per front; write dev.json
  test  first --limit holdout songs by SHA-256 not seen; X+V vs X+V+R (round 4) vs X+V+R2; write test.json

Scoring: family majmin, Chromatic = no correction. Preregistration: preregistration.md, section frontend-b-05.
"""
import argparse
import itertools
import json
import sys
from pathlib import Path

import numpy as np

import recent_vocal
from run_frontend_b import compare, kv, songs_in, summarize
from run_recent_vocal import FRONTS, mean, oracle_mispull, score, setup

W_GRID = [1.0, 2.0, 4.0]
H_GRID = [20.0, 40.0]
D_GRID = [10, 20, 30]
TOL_GRID = [0.0, 0.05]
N_GRID = [0]  # onset delay; R3 overrides via --onset
X_GRID = [0]  # dwell enabled after X s of mix evidence; R4 overrides via --dwell-after


def combos(song, front, params, w=0.0, h=20.0, dwell=0, tol=None, onset=0, dwell_after=0):
    duration = float(song["data"]["duration"])
    base, recent, recent_mix = recent_vocal.streams(song["views"][FRONTS[front]], song["gen_times"],
                                                    song["gen_notes"], duration, params[front], h)
    active_from = recent_vocal.onset_step(song["gen_times"], song["gen_notes"], duration, onset)
    dwell_from = recent_vocal.mix_seconds_step(song["views"][FRONTS[front]], duration, params[front], dwell_after)
    return recent_vocal.track(base, recent, params[front], w, dwell, tol, recent_mix, active_from, dwell_from)


def stats(results):
    return {"mispull": mean([r["whole"]["mispull"] for r in results]), "combined": mean([r["combined"] for r in results]),
            "firstLine": mean([r["firstLineCombined"] for r in results]),
            "switches": mean([r["switches"] for r in results]),
            "maxSwitches": int(max(r["switches"] for r in results)),
            "songsOver5pct": sum(r["whole"]["mispull"] > 0.05 for r in results)}


def dev(args):
    params, seen = setup(args)
    caches = kv(args.b_cache)
    songs = (songs_in(args.songs, args.cache, caches, "pilot")[0]
             + songs_in(args.songs, args.cache, caches, "holdout", only=seen)[0])
    report = {"songs": [s["name"] for s in songs],
              "grid": {"w": W_GRID, "h": H_GRID, "dwell": D_GRID, "tol": TOL_GRID, "onset": N_GRID,
                       "dwellAfter": X_GRID},
              "fronts": {}}
    for front in FRONTS:
        ref = stats([score(s, combos(s, front, params)) for s in songs])
        cells = []
        for x, n, w, h, d, tol in itertools.product(X_GRID, N_GRID, W_GRID, H_GRID, D_GRID, TOL_GRID):
            cell = {"w": w, "h": h, "dwell": d, "tol": tol, "onset": n, "dwellAfter": x,
                    **stats([score(s, combos(s, front, params, w, h, d, tol, n, x)) for s in songs])}
            cell["eligible"] = (cell["combined"] >= ref["combined"] and cell["firstLine"] >= ref["firstLine"]
                                and cell["switches"] <= ref["switches"])
            cells.append(cell)
            print(front, cell, flush=True)
        eligible = [c for c in cells if c["eligible"]]
        # min mis-pull; ties: smaller w, larger D, larger h, smaller tol, smaller onset (preregistered)
        chosen = (min(eligible, key=lambda c: (round(c["mispull"], 6), c["w"], -c["dwell"], -c["h"], c["tol"],
                                               c.get("onset", 0), c.get("dwellAfter", 0)))
                  if eligible else None)
        report["fronts"][front] = {"baseline": ref, "cells": cells, "chosen": chosen}
        print(front, "baseline", ref, "chosen", chosen, flush=True)
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=False)
    (out / "dev.json").write_text(json.dumps(report, ensure_ascii=False, indent=1), encoding="utf-8")


def test(args):
    params, seen = setup(args)
    r2 = {f: v["chosen"] for f, v in json.loads(Path(args.dev).read_text(encoding="utf-8"))["fronts"].items()}
    r1 = {f: v["chosen"] for f, v in json.loads(Path(args.r_dev).read_text(encoding="utf-8"))["fronts"].items()}
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=False)
    songs, excluded = songs_in(args.songs, args.cache, kv(args.b_cache), "holdout", args.limit, skip=seen)
    rows = []
    for s in songs:
        res = {}
        for front in FRONTS:
            res[front] = score(s, combos(s, front, params))
            if r1[front]:
                res[front + "+R"] = score(s, combos(s, front, params, r1[front]["w"], r1[front]["h"]))
            if r2[front]:
                c = r2[front]
                res[front + args.label] = score(s, combos(s, front, params, c["w"], c["h"], c["dwell"], c["tol"],
                                                          c.get("onset", 0), c.get("dwellAfter", 0)))
        oracle, oracle_key = oracle_mispull(s)
        rows.append({"name": s["name"], "sha256": s["sha256"], "duration": round(float(s["data"]["duration"]), 1),
                     "oracleMispull": oracle, "oracleKey": oracle_key, "strategies": res})
        print("done", s["name"], {k: (v["whole"]["mispull"], v["combined"], v["switches"]) for k, v in res.items()},
              flush=True)
    labels = list(rows[0]["strategies"])
    lab = args.label
    primary = [(f + lab, f) for f in FRONTS if r2[f]]
    aux = [(f + lab, f + "+R") for f in FRONTS if r2[f] and r1[f]] + ([("B2+V" + lab, "A+V")] if r2["B2+V"] else [])
    report = {"preregistration": args.prereg, "chosen" + lab.strip("+"): r2, "chosenR": r1,
              "excluded": excluded, "songs": rows, "summary": {l: summarize(rows, l) for l in labels},
              "maxSwitches": {l: max(r["strategies"][l]["switches"] for r in rows) for l in labels},
              "avoidableMispull": {l: mean([r["strategies"][l]["whole"]["mispull"] - r["oracleMispull"] for r in rows])
                                   for l in labels},
              "oracleMispull": mean([r["oracleMispull"] for r in rows]),
              "comparisons": {f"{n} vs {b}": compare(rows, n, b) for n, b in primary + aux}}
    for n, b in primary:
        report["verdict " + n] = report["comparisons"][f"{n} vs {b}"]["improved"]
    (out / "test.json").write_text(json.dumps(report, ensure_ascii=False, indent=1), encoding="utf-8")
    print("\nlabel      songs mispull  >5%  combined firstLine switches maxSw avoidable")
    for l, s in report["summary"].items():
        print(f"{l:10s} {s['songs']:5d} {s['mispull']:.4f} {s['songsMispullOver5pct']:4d}   {s['combined']:.4f}   "
              f"{s['firstLineCombined']:.4f}   {s['switches']:5.2f} {report['maxSwitches'][l]:5d}  "
              f"{report['avoidableMispull'][l]:.4f}")
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
    parser.add_argument("--dev", help="test: R2 dev.json")
    parser.add_argument("--r-dev", help="test: round-4 R dev.json (auxiliary comparison)")
    parser.add_argument("--onset", type=int, action="append", help="dev: onset delays N (R3); default [0]")
    parser.add_argument("--dwell-after", type=int, action="append", help="dev: X grid (R4); default [0]")
    parser.add_argument("--label", default="+R2", help="test: label suffix for the mechanism")
    parser.add_argument("--prereg", default="preregistration.md#frontend-b-05")
    parser.add_argument("--limit", type=int, default=10)
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    sys.stdout.reconfigure(encoding="utf-8")
    if args.onset:
        N_GRID[:] = args.onset
    if args.dwell_after:
        X_GRID[:] = args.dwell_after
    dev(args) if args.command == "dev" else test(args)


if __name__ == "__main__":
    main()
