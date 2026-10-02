"""Front end A (chroma_cqt) vs candidate front end B (frontend_b.py) under the same matcher.

  calibrate  choose one B revision's matcher params on the 5 pilot songs only (same grid/objective as A)
  evaluate   score A, A+V (frozen) and every given B revision (+V) on the first --limit holdout songs by
             SHA-256, skipping songs already used in --exclude-report files; write frontend-b.json

Revisions (frontend_b.REVISIONS): b1 = frontend-b-01; b2 = tuning estimate limited to resolvable bins.
Repeated options take REV=VALUE, e.g. --b-cache b1=DIR --b-cache b2=DIR --params b2=FILE.
Preregistration: work/scale-match-20261002/preregistration.md, sections frontend-b-01 and frontend-b-02.
"""
import argparse
import dataclasses
import itertools
import json
import sys
from pathlib import Path

import numpy as np

import frontend_b
import lrc
import matcher
import run_pilot
from features import load_song, sha256
from metrics import success_table
from run_holdout import FROZEN, MIN_VOICED_SECONDS, PILOT, PILOT_TITLES, first_vocal_line
from run_pilot import GRID, STEP, evaluate, harmony_at, matcher_combos, stemgen_vocal_notes, vocal_truth

BASE = {"A": ("A", "S1"), "A+V": ("A", "S3g")}  # label -> (front end, matcher source); "V" = StemgenRT vocal notes


def strategies(revisions):
    out = dict(BASE)
    for rev in revisions:
        out[rev.upper()] = (rev, "S1")
        out[rev.upper() + "+V"] = (rev, "S3g")
    return out


def pairs(revisions):
    """Primary pairs first (last revision vs A); then newest vs previous revision, auxiliary."""
    revs = [r.upper() for r in reversed(revisions)]
    out = [(r + v, "A" + v) for r in revs for v in ("", "+V")]
    if len(revs) > 1:
        out += [(revs[0] + v, revs[1] + v) for v in ("", "+V")]
    return out


def kv(items):
    return dict(i.split("=", 1) for i in items or [])


def group_of(path, digest):
    if digest[:16] in PILOT:
        return "pilot"
    return "pilot-duplicate" if path.stem.rsplit(" - ", 1)[0] in PILOT_TITLES else "holdout"


def prepare(path, cache_root, b_caches):
    try:
        digest, data = load_song(path, cache_root)
    except Exception as exc:  # undecodable file (preregistered exclusion)
        return dict(name=path.name, sha256=sha256(path), group=group_of(path, sha256(path)), stats=None), \
            f"undecodable: {type(exc).__name__}"
    times, notes, stats, free_times, free_sung = vocal_truth(data)
    song = dict(name=path.name, sha256=digest, group=group_of(path, digest), stats=stats)
    if stats["voicedSeconds"] < MIN_VOICED_SECONDS:
        return song, "little or no vocal"
    lrc_path = path.with_suffix(".lrc")
    if lrc_path.exists():
        first, source = lrc.first_line(lrc.parse(lrc_path.read_text(encoding="utf-8-sig"))), "lrc"
    else:
        first, source = first_vocal_line(data["f0_t"], np.concatenate([times, free_times])), "vocal-activity"
    if first is None:
        return song, "no sustained vocal onset"
    gen_times, gen_notes = stemgen_vocal_notes(Path(cache_root) / digest[:16], "stemgenrt")
    if gen_times is None:
        return song, "no StemgenRT vocal stem"
    views, tuning = {"A": data}, {}
    for rev, root in b_caches.items():
        b = frontend_b.load(path, Path(root) / f"{digest[:16]}.npz", rev)
        views[rev] = dict(data, mix_t=b["b_t"], mix_c=b["b_c"], mix_rms=b["b_rms"])
        tuning[rev] = float(b["b_delta"][-1])
    song.update(data=data, views=views, b_tuning=tuning, times=times, notes=notes,
                free_times=free_times, free_sung=free_sung, harmony=harmony_at(data, free_times),
                first=first, first_source=source, gen_times=gen_times, gen_notes=gen_notes)
    return song, None


def combos_for(song, front, source, params):
    view = dict(song, data=song["views"][front])
    return matcher_combos(view, params, source)


def first_narrowing(combos):
    """Seconds until the first non-Chromatic step (combos[k] is active during [k, k+1) s); None = never."""
    hit = np.flatnonzero(combos != matcher.CHROMATIC)
    return float(hit[0] * STEP) if len(hit) else None


def score(song, combos):
    result = evaluate(song, combos)
    result["firstNarrowing"] = first_narrowing(combos)
    return result


def songs_in(folder, cache, b_caches, group, limit=None, skip=(), only=None):
    """Songs of one group in SHA-256 order; the first `limit` not in `skip` (and in `only`, if given)
    that pass every exclusion."""
    paths = sorted(Path(folder).glob("*.mp3"))
    digests = {p: sha256(p) for p in paths}
    kept, excluded = [], []
    for path in sorted(paths, key=lambda p: digests[p]):
        if group_of(path, digests[path]) != group or digests[path] in skip:
            continue
        if only is not None and digests[path] not in only:
            continue
        if limit is not None and len(kept) >= limit:
            break
        song, reason = prepare(path, cache, b_caches)
        if reason:
            excluded.append({"name": song["name"], "sha256": song["sha256"], "group": song["group"],
                             "reason": reason, "truth": song["stats"]})
        else:
            kept.append(song)
        print("loaded", path.name, reason or "", flush=True)
    return kept, excluded


def calibrate(args):
    rev = args.revision
    songs, _ = songs_in(args.songs, args.cache, {rev: kv(args.b_cache)[rev]}, "pilot")
    assert len(songs) == 5, [s["name"] for s in songs]
    grid = [matcher.Params(**dict(zip(GRID, v)), family="diatonic")
            for v in itertools.product(*[GRID[k] for k in GRID if k != "family"])]
    chosen = {}
    for label, source in ((rev.upper(), "S1"), (rev.upper() + "+V", "S3g")):
        objective = [np.mean([1.0 - (evaluate(s, combos_for(s, rev, source, p))["combined"] or 0.0) for s in songs])
                     for p in grid]
        best = int(np.argmin(objective))  # first of equal minima = grid order
        chosen[label] = {"params": grid[best].__dict__, "meanObjective": round(float(objective[best]), 5),
                         "gridSize": len(grid)}
        print(label, chosen[label], flush=True)
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=False)
    (out / "params.json").write_text(json.dumps({"revision": rev, "pilotSongs": [s["name"] for s in songs],
                                                 "chosen": chosen},
                                                ensure_ascii=False, indent=1), encoding="utf-8")


def mean(xs):
    xs = [x for x in xs if x is not None]
    return float(np.mean(xs)) if xs else None


def bootstrap(diff, seed=0, n=10_000):
    diff = np.asarray([d for d in diff if d is not None], dtype=float)
    if not len(diff):
        return None
    rng = np.random.default_rng(seed)
    means = diff[rng.integers(0, len(diff), (n, len(diff)))].mean(axis=1)
    return [round(float(np.percentile(means, 2.5)), 4), round(float(np.percentile(means, 97.5)), 4)]


def summarize(rows, label):
    res = [r["strategies"][label] for r in rows if label in r["strategies"]]
    narrow = [x["firstNarrowing"] for x in res]
    return {
        "songs": len(res),
        "mispull": mean([x["whole"]["mispull"] for x in res]),
        "songsMispullOver5pct": sum(x["whole"]["mispull"] is not None and x["whole"]["mispull"] > 0.05 for x in res),
        "combined": mean([x["combined"] for x in res]),
        "firstLineCombined": mean([x["firstLineCombined"] for x in res]),
        "firstLineMispull": mean([x["firstLine"]["mispull"] for x in res]),
        "switches": mean([x["switches"] for x in res]),
        # never-narrowed songs count as the full song length (preregistered)
        "firstNarrowingSeconds": mean([n if n is not None else r["duration"]
                                       for n, r in zip(narrow, [r for r in rows if label in r["strategies"]])]),
        "neverNarrowed": sum(n is None for n in narrow),
        "narrowedBeforeFirstLine": sum(x["narrowedBeforeFirstLine"] for x in res),
    }


def compare(rows, new, base):
    both = [r for r in rows if new in r["strategies"] and base in r["strategies"]]
    a, b = [r["strategies"][base] for r in both], [r["strategies"][new] for r in both]
    s_new, s_base = summarize(both, new), summarize(both, base)
    d_comb = [y["combined"] - x["combined"] if None not in (x["combined"], y["combined"]) else None for x, y in zip(a, b)]
    d_first = [y["firstLineCombined"] - x["firstLineCombined"]
               if None not in (x["firstLineCombined"], y["firstLineCombined"]) else None for x, y in zip(a, b)]
    checks = {
        "mispullNotHigher": s_new["mispull"] <= s_base["mispull"],
        "combinedHigher": s_new["combined"] > s_base["combined"],
        "firstLineHigher": s_new["firstLineCombined"] > s_base["firstLineCombined"],
        "switchesNotMore": s_new["switches"] <= s_base["switches"],
    }
    return {
        "songs": len(both), new: s_new, base: s_base, "checks": checks, "improved": all(checks.values()),
        "pairedCombined": {"wins": sum(d > 0 for d in d_comb if d is not None),
                           "losses": sum(d < 0 for d in d_comb if d is not None), "ci95": bootstrap(d_comb)},
        "pairedFirstLine": {"wins": sum(d > 0 for d in d_first if d is not None),
                            "losses": sum(d < 0 for d in d_first if d is not None), "ci95": bootstrap(d_first)},
    }


def evaluate_all(args):
    caches, param_files = kv(args.b_cache), kv(args.params)
    revisions = list(caches)
    assert set(param_files) == set(revisions), (param_files, revisions)
    params = {"A": FROZEN["S1"], "A+V": FROZEN["S3g"]}
    for rev, file in param_files.items():
        chosen = json.loads(Path(file).read_text(encoding="utf-8"))["chosen"]
        for suffix in ("", "+V"):  # b1's params.json predates revisions and labels entries "B" / "B+V"
            entry = chosen.get(rev.upper() + suffix) or chosen["B" + suffix]
            params[rev.upper() + suffix] = matcher.Params(**entry["params"])
    if args.family:  # structural override only (e.g. "majmin": never Chromatic); other params unchanged
        params = {k: dataclasses.replace(v, family=args.family) for k, v in params.items()}
    if args.chromatic_no_correction:  # Chromatic / unwritten = the sung pitch passes through uncorrected
        run_pilot.TABLE = success_table(matcher.MASKS, chromatic_corrects=False)
    def shas(files):
        return {s["sha256"] for f in files or [] for s in json.loads(Path(f).read_text(encoding="utf-8"))["songs"]}
    skip, only = shas(args.exclude_report), (shas(args.only_report) if args.only_report else None)
    labels, comparisons = strategies(revisions), pairs(revisions)
    if args.cached:  # second play: the first play's final choice is written from t = 0
        comparisons += [(n + "-cached", b + "-cached") for n, b in comparisons if b.startswith("A")]
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=False)
    songs, excluded = songs_in(args.songs, args.cache, caches, "holdout", None if only else args.limit, skip, only)
    rows = []
    for song in songs:
        res = {}
        for label, (front, source) in labels.items():
            combos = combos_for(song, front, source, params[label])
            res[label] = score(song, combos)
            if args.cached:
                res[label + "-cached"] = score(song, np.full(len(combos), combos[-1]))
        rows.append({"name": song["name"], "sha256": song["sha256"], "group": song["group"],
                     "duration": round(float(song["data"]["duration"]), 1), "firstLine": song["first"],
                     "firstLineSource": song["first_source"], "truth": song["stats"],
                     "bTuningCents": {r: round(d * 100, 1) for r, d in song["b_tuning"].items()},
                     "strategies": res})
        print("done", song["name"], {k: v["combined"] for k, v in res.items()}, flush=True)
    groups = {"sample": rows}  # holdout songs only (first --limit by SHA-256 after skips)
    if args.cached:
        labels = {**labels, **{k + "-cached": v for k, v in labels.items()}}
    report = {"schemaVersion": 3, "preregistration": args.prereg, "revisions": revisions,
              "skippedFrom": args.exclude_report or [], "onlyFrom": args.only_report or [],
              "family": args.family, "chromaticCorrects": not args.chromatic_no_correction,
              "params": {k: v.__dict__ for k, v in params.items()}, "excluded": excluded, "songs": rows,
              "summary": {g: {label: summarize(rs, label) for label in labels} for g, rs in groups.items()},
              "comparisons": {g: {f"{n} vs {b}": compare(rs, n, b) for n, b in comparisons}
                              for g, rs in groups.items()}}
    report["tuningCheck"] = {rev: tuning_check(rows, rev) for rev in revisions}
    for n, b in comparisons:
        report["verdict " + n] = all(report["comparisons"][g][f"{n} vs {b}"]["improved"] for g in groups)
    (out / "frontend-b.json").write_text(json.dumps(report, ensure_ascii=False, indent=1), encoding="utf-8")
    print_summary(report)


def tuning_check(rows, rev):
    truth = np.array([r["truth"]["tuningOffsetCents"] for r in rows])
    est = np.array([r["bTuningCents"][rev] for r in rows])
    err = (est - truth + 50) % 100 - 50
    return {"medianAbsErrorCents": round(float(np.median(np.abs(err))), 1),
            "meanErrorCents": round(float(np.mean(err)), 1),
            "pearson": round(float(np.corrcoef(truth, est)[0, 1]), 3) if len(rows) > 2 else None}


def print_summary(report):
    for g, table in report["summary"].items():
        print(f"\n[{g}]  label  songs  mispull  >5%  combined  firstLine  switches  firstNarrow(s)  never  preFirstLine")
        for label, s in table.items():
            print(f"        {label:5s} {s['songs']:5d}  {s['mispull']:.4f}  {s['songsMispullOver5pct']:3d}  "
                  f"{s['combined']:.4f}    {s['firstLineCombined']:.4f}    {s['switches']:5.2f}  "
                  f"{s['firstNarrowingSeconds']:13.1f}  {s['neverNarrowed']:5d}  {s['narrowedBeforeFirstLine']:5d}")
        for name, c in report["comparisons"][g].items():
            print(f"  {name}: improved={c['improved']} {c['checks']} "
                  f"combined W/L {c['pairedCombined']['wins']}/{c['pairedCombined']['losses']} CI {c['pairedCombined']['ci95']}; "
                  f"firstLine W/L {c['pairedFirstLine']['wins']}/{c['pairedFirstLine']['losses']} CI {c['pairedFirstLine']['ci95']}")
    print("\ntuning", report["tuningCheck"])
    print({k: v for k, v in report.items() if k.startswith("verdict")})


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("command", choices=["calibrate", "evaluate"])
    parser.add_argument("--songs", required=True)
    parser.add_argument("--cache", required=True)
    parser.add_argument("--b-cache", action="append", required=True, help="REV=DIR")
    parser.add_argument("--out", required=True)
    parser.add_argument("--revision", default="b2", help="calibrate: which B revision")
    parser.add_argument("--params", action="append", help="evaluate: REV=params.json")
    parser.add_argument("--exclude-report", action="append", help="evaluate: skip songs scored in this report")
    parser.add_argument("--only-report", action="append", help="evaluate: score exactly the songs of this report")
    parser.add_argument("--family", help="evaluate: override every strategy's family (e.g. majmin)")
    parser.add_argument("--chromatic-no-correction", action="store_true", help="evaluate: Chromatic = no correction")
    parser.add_argument("--cached", action="store_true", help="evaluate: also score the cache-hit replay")
    parser.add_argument("--prereg", default="preregistration.md#frontend-b-02")
    parser.add_argument("--limit", type=int, default=10)
    args = parser.parse_args()
    sys.stdout.reconfigure(encoding="utf-8")
    calibrate(args) if args.command == "calibrate" else evaluate_all(args)


if __name__ == "__main__":
    main()
