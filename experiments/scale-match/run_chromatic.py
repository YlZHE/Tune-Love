"""chromatic-gate-01: calibrate the Chromatic-when-uncertain gate on used songs, then test it on unused songs.

  dev   grid over (ENTER, EXIT, EXIT_HOLD_SECONDS, MIN_SECONDS) on every song used in an earlier round
  test  frozen values vs the current majmin production path on the first --limit never-used songs (SHA-256 order)
  used  list which files of --songs were used in earlier rounds (from the evidence json files)

Metric (same as run_nochromatic): Chromatic and "nothing written yet" = no correction.
Preregistration: work/scale-match-20261002/preregistration.md, section chromatic-gate-01.
"""
import argparse
import itertools
import json
import sys
from pathlib import Path

import numpy as np

import lrc
import matcher
import run_nochromatic  # noqa: F401  (sets run_pilot.TABLE to the Chromatic-does-not-correct table)
from chromatic_gate import GateParams, production_chromatic
from features import load_song, sha256
from replay_production import probe_rows
from run_holdout import MIN_VOICED_SECONDS, PILOT_TITLES, first_vocal_line
from run_nochromatic import cached, production_majmin
from run_pilot import PROBE, active_at, evaluate, harmony_at, vocal_truth

ENTER = [0.06, 0.08, 0.10, 0.12, 0.15]
EXIT = [0.03, 0.05, 0.07, 0.09]
EXIT_HOLD = [3.0, 6.0, 10.0]
MIN_SECONDS = [3.0, 6.0, 10.0]
GRID = [GateParams(e, x, h, m) for e, x, h, m in itertools.product(ENTER, EXIT, EXIT_HOLD, MIN_SECONDS) if x < e]
TIE = 0.002  # 0.2 percentage points of mispull


def title(path):
    return path.stem.rsplit(" - ", 1)[0]


def used_hashes(evidence_root):
    """SHA-256 of every song scored in any earlier round (pilot-*, holdout-*, nochromatic, replay, frontend-b-*)."""
    out = set()
    for f in Path(evidence_root).glob("*/*.json"):
        data = json.loads(f.read_text(encoding="utf-8"))
        if isinstance(data, dict) and isinstance(data.get("songs"), list):
            out |= {s["sha256"] for s in data["songs"] if isinstance(s, dict) and s.get("sha256")}
    return out


def is_used(path, used):
    return sha256(path) in used or title(path) in PILOT_TITLES


def load_scored(path, cache):
    """Song dict ready for evaluate(), or (None, reason) when a preregistered exclusion applies."""
    try:
        digest, data = load_song(path, cache)
    except Exception as exc:  # undecodable file
        return None, f"undecodable: {type(exc).__name__}"
    times, notes, stats, free_times, free_sung = vocal_truth(data)
    if stats["voicedSeconds"] < MIN_VOICED_SECONDS:
        return None, "little or no vocal"
    lrc_path = path.with_suffix(".lrc")
    first = (lrc.first_line(lrc.parse(lrc_path.read_text(encoding="utf-8-sig"))) if lrc_path.exists()
             else first_vocal_line(data["f0_t"], np.concatenate([times, free_times])))
    if first is None:
        return None, "no sustained vocal onset"
    song = dict(name=path.name, sha256=digest, data=data, times=times, notes=notes, free_times=free_times,
                free_sung=free_sung, harmony=harmony_at(data, free_times), first=first)
    song["rows"] = probe_rows(PROBE, path, Path(cache) / digest[:16])
    return song, None


def score(song, combos):
    r = evaluate(song, combos)
    share = float((combos[1:] == matcher.CHROMATIC).mean())
    return {"mispull": r["whole"]["mispull"], "frames": r["whole"]["frames"],
            "coverage": float((active_at(song["times"], combos) != matcher.CHROMATIC).mean()),
            "chromaticShare": share, "allChromatic": share == 1.0, "combined": r["combined"],
            "firstLineCombined": r["firstLineCombined"], "firstLineMispull": r["firstLine"]["mispull"],
            "switches": r["switches"], "finalCombo": r["finalCombo"]}


def mean(xs):
    xs = [x for x in xs if x is not None]
    return float(np.mean(xs)) if xs else None


METRICS = ("mispull", "coverage", "chromaticShare", "combined", "firstLineCombined", "firstLineMispull", "switches")


def aggregate(per_song):
    out = {m: mean([s[m] for s in per_song]) for m in METRICS}
    out["songs"] = len(per_song)
    out["mispullOver5pct"] = sum(s["mispull"] is not None and s["mispull"] > 0.05 for s in per_song)
    out["allChromatic"] = sum(s["allChromatic"] for s in per_song)
    return out


def gate_tracks(song, params):
    first = production_chromatic(song["rows"], float(song["data"]["duration"]), params)
    return {"first": first, "cached": cached(first)}


def baseline_tracks(song):
    first = production_majmin(song["rows"], float(song["data"]["duration"]))
    return {"first": first, "cached": cached(first)}


def choose(table):
    """Preregistered rule: lowest dev mispull; within 0.2 pp of it, highest coverage; then grid order."""
    best = min(t["first"]["mispull"] for t in table)
    near = [t for t in table if t["first"]["mispull"] <= best + TIE]
    return max(near, key=lambda t: (t["first"]["coverage"], -table.index(t)))


def dev(args):
    songs, excluded = [], []
    for path in sorted(Path(args.songs).glob("*.mp3")):
        if sha256(path) not in args.used:  # same-title duplicates of pilot songs stay out, as in earlier rounds
            excluded.append({"name": path.name, "reason": "not scored in an earlier round"})
            continue
        song, reason = load_scored(path, args.cache)
        if song is None:
            excluded.append({"name": path.name, "reason": reason})
            continue
        songs.append(song)
        print("loaded", path.name, flush=True)
    base = [{k: score(s, v) for k, v in baseline_tracks(s).items()} for s in songs]
    memo, table = {}, []
    for params in GRID:
        per = []
        for i, s in enumerate(songs):
            tracks = gate_tracks(s, params)
            per.append({k: memo.setdefault((i, k, v.tobytes()), score(s, v)) for k, v in tracks.items()})
        table.append({"params": params._asdict(), **{k: aggregate([p[k] for p in per]) for k in ("first", "cached")}})
        print("grid", tuple(params), table[-1]["first"]["mispull"], table[-1]["first"]["coverage"], flush=True)
    return {"songs": [s["name"] for s in songs], "excluded": excluded,
            "majmin": {k: aggregate([b[k] for b in base]) for k in ("first", "cached")},
            "grid": table, "chosen": choose(table)}


def test(args):
    params = GateParams(args.enter, args.exit, args.exit_hold, args.min_seconds)
    kept, excluded = [], []
    for path in sorted(Path(args.songs).glob("*.mp3"), key=sha256):
        if len(kept) >= args.limit:
            break
        if is_used(path, args.used):
            continue
        song, reason = load_scored(path, args.cache)
        if song is None:
            excluded.append({"name": path.name, "reason": reason})
        else:
            kept.append(song)
        print("loaded", path.name, reason or "", flush=True)
    rows = [{"name": s["name"], "sha256": s["sha256"], "firstLine": s["first"],
             "gate": {k: score(s, v) for k, v in gate_tracks(s, params).items()},
             "majmin": {k: score(s, v) for k, v in baseline_tracks(s).items()}} for s in kept]
    summary = {a: {k: aggregate([r[a][k] for r in rows]) for k in ("first", "cached")}
               for a in ("gate", "majmin")} if rows else {}
    return {"params": params._asdict(), "songs": rows, "excluded": excluded, "summary": summary,
            "allChromaticSongs": [r["name"] for r in rows if r["gate"]["first"]["allChromatic"]]}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=["dev", "test", "used"])
    parser.add_argument("--songs", required=True)
    parser.add_argument("--cache", required=True)
    parser.add_argument("--evidence", required=True, help="evidence root holding earlier rounds' json files")
    parser.add_argument("--out", help="output directory (created exclusively)")
    parser.add_argument("--limit", type=int, default=10)
    for name in ("enter", "exit", "exit-hold", "min-seconds"):
        parser.add_argument("--" + name, type=float)
    args = parser.parse_args()
    sys.stdout.reconfigure(encoding="utf-8")
    args.used = used_hashes(args.evidence)
    if args.mode == "used":
        for p in sorted(Path(args.songs).glob("*.mp3"), key=sha256):
            print("used  " if is_used(p, args.used) else "UNUSED", sha256(p)[:16], p.name)
        return
    if args.mode == "test" and None in (args.enter, args.exit, args.exit_hold, args.min_seconds):
        parser.error("test needs the frozen values: --enter --exit --exit-hold --min-seconds")
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=False)
    result = dev(args) if args.mode == "dev" else test(args)
    (out / f"chromatic-gate-{args.mode}.json").write_text(json.dumps(result, ensure_ascii=False, indent=1),
                                                          encoding="utf-8")
    print(json.dumps(result.get("chosen") or result.get("summary"), indent=1))


if __name__ == "__main__":
    main()
