"""Can ReccoBeats supply a usable Key/Scale before the vocal starts? (user approved 2026-10-02)

For each holdout song: search by title (and title + artist), pick a candidate whose artist
matches and whose duration is within 3 s, fetch audio-features (key, mode), and score:
  - hit / correct-recording rates, cold latency
  - same pitch-class set as the offline best fixed Major/Minor (S5) for the song
  - first-line and whole-song output correctness when the external result is written
    at t = 0 and the production analysis takes over after the 20 s seed hold
Requests are rate-limited to <= 1/s and every response is cached under the evidence dir.
"""
import argparse
import json
import re
import sys
import time
import unicodedata
import urllib.parse
import urllib.request
from pathlib import Path

import numpy as np

import matcher
import run_nochromatic as base  # sets the no-Chromatic metric table
from features import load_song
from run_pilot import STEP, evaluate, harmony_at, vocal_truth
from scales import KEYS, mask

API = "https://api.reccobeats.com/v1"
HOLD_SECONDS = 20.0
last_call = [0.0]


def get(url, cache_file):
    if cache_file.exists():
        return json.loads(cache_file.read_text(encoding="utf-8"))
    wait = 1.05 - (time.time() - last_call[0])
    if wait > 0:
        time.sleep(wait)
    started = time.time()
    try:
        # Python's default User-Agent is rejected (403); identify the client honestly.
        headers = {"Accept": "application/json",
                   "User-Agent": "AutoTuneHelper-research/0.1 (non-commercial; key lookup evaluation)"}
        with urllib.request.urlopen(urllib.request.Request(url, headers=headers), timeout=15) as r:
            body, status = json.loads(r.read().decode("utf-8")), r.status
    except urllib.error.HTTPError as e:
        body, status = None, e.code
    except Exception as e:  # network error
        body, status = None, f"error: {type(e).__name__}"
    last_call[0] = time.time()
    record = {"status": status, "latencyMs": round((last_call[0] - started) * 1000), "body": body, "url": url}
    cache_file.write_text(json.dumps(record, ensure_ascii=False), encoding="utf-8")
    return record


def norm(s):
    s = unicodedata.normalize("NFKC", s).lower()
    return re.sub(r"[\s\-_.,'’&!?()\[\]（）【】]+", "", s)


def parse_name(stem):
    title, artist = (stem.rsplit(" - ", 1) + [""])[:2] if " - " in stem else (stem, "")
    title = re.sub(r"\s*[\(（]\s*prod[^)）]*[\)）]", "", title, flags=re.I).strip()
    artists = [a for a in re.split(r"[&,、/]| feat\.? | ft\.? ", artist, flags=re.I) if a.strip()]
    return title, artists


def pick(candidates, title, artists, duration):
    best = None
    for c in candidates or []:
        names = [norm(a.get("name", "")) for a in c.get("artists", [])]
        artist_ok = any(norm(a) and (norm(a) in n or n in norm(a)) for a in artists for n in names if n)
        title_ok = norm(c.get("trackTitle", "")) == norm(title) or norm(title) in norm(c.get("trackTitle", ""))
        dur_ok = c.get("durationMs") is not None and abs(c["durationMs"] / 1000 - duration) <= 3.0
        score = (artist_ok, dur_ok, title_ok)
        if title_ok and (artist_ok or dur_ok) and (best is None or score > best[0]):
            best = (score, c)
    return best


def seeded_combos(rows, duration, seed_idx):
    """Mirror Rust: start from the seed, hold it for 20 s of evidence, then normal hysteresis."""
    steps = int(np.ceil(duration / STEP))
    params = base.MAJMIN["S1"]
    tracker = matcher.Tracker(params)
    tracker.current = seed_idx
    seeded = True
    evidence = np.zeros(12)
    combos = np.full(steps + 1, seed_idx)
    by_second = {r["second"]: r["evidence"] for r in rows if r["evidence"]}
    for k in range(1, steps + 1):
        ev = by_second.get(k)
        if ev and ev["seconds"] > 0 and ev["rms"] >= 1e-3:
            c = np.array(ev["chroma"], dtype=float)
            if c.max() > 0:
                shaped = (c / c.max()) ** params.gamma
                evidence += shaped / shaped.sum() * ev["seconds"]
        if not (seeded and evidence.sum() < HOLD_SECONDS):
            before = tracker.current
            tracker.update(evidence)
            if tracker.current != before:
                seeded = False
        combos[k] = tracker.current
    return combos


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--songs", required=True)
    parser.add_argument("--cache", required=True)
    parser.add_argument("--holdout", required=True, help="nochromatic.json from the re-analysis")
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    sys.stdout.reconfigure(encoding="utf-8")
    out = Path(args.out)
    (out / "responses").mkdir(parents=True, exist_ok=True)
    holdout = json.loads(Path(args.holdout).read_text(encoding="utf-8"))
    combo_index = {matcher.COMBOS[i]: i for i in range(len(matcher.COMBOS))}
    results = []
    for entry in holdout["songs"]:
        path = Path(args.songs) / entry["name"]
        digest, data = load_song(path, args.cache)
        duration = float(data["duration"])
        title, artists = parse_name(path.stem)
        tag = digest[:16]
        queries = [title + (" " + artists[0] if artists else ""), title]
        chosen, latency, searched = None, [], []
        for qi, q in enumerate(dict.fromkeys(queries)):
            url = f"{API}/track/search?searchText={urllib.parse.quote(q)}&size=25"
            rec = get(url, out / "responses" / f"{tag}-search{qi}.json")
            latency.append(rec["latencyMs"])
            content = (rec["body"] or {}).get("content", []) if isinstance(rec["body"], dict) else []
            searched.append({"query": q, "status": rec["status"], "hits": len(content)})
            chosen = pick(content, title, artists, duration)
            if chosen:
                break
        row = {"name": entry["name"], "title": title, "artists": artists, "searches": searched,
               "searchLatencyMs": latency, "match": None}
        if chosen:
            (artist_ok, dur_ok, _), cand = chosen
            rec = get(f"{API}/track/{cand['id']}/audio-features", out / "responses" / f"{tag}-features.json")
            feat = rec["body"] if isinstance(rec["body"], dict) else None
            row["match"] = {"id": cand["id"], "title": cand.get("trackTitle"),
                            "artists": [a.get("name") for a in cand.get("artists", [])],
                            "durationS": (cand.get("durationMs") or 0) / 1000, "artistMatch": artist_ok,
                            "durationMatch": dur_ok, "featuresStatus": rec["status"],
                            "featuresLatencyMs": rec["latencyMs"],
                            "key": feat.get("key") if feat else None, "mode": feat.get("mode") if feat else None}
        results.append(row)
        print(entry["name"], "->", row["match"] and (row["match"]["title"], row["match"]["key"], row["match"]["mode"]), flush=True)

    # Score against the offline best fixed Major/Minor and simulate seeding.
    for row, entry in zip(results, holdout["songs"]):
        oracle = entry["strategies"]["S5-majmin"]["finalCombo"]
        k, s = oracle.split(" ", 1)
        oracle_mask = tuple(mask(KEYS.index(k), s))
        m = row["match"]
        ext = None
        if m and isinstance(m.get("key"), int) and 0 <= m["key"] < 12 and m.get("mode") in (0, 1):
            ext = (m["key"], "Major" if m["mode"] == 1 else "Minor")
        row["external"] = ext and f"{KEYS[ext[0]]} {ext[1]}"
        row["oracle"] = oracle
        row["sameSetAsOracle"] = bool(ext) and tuple(mask(*ext)) == oracle_mask
        row["sameTonicAsOracle"] = bool(ext) and ext == (KEYS.index(k), s)
        if ext:
            path = Path(args.songs) / entry["name"]
            digest, data = load_song(path, args.cache)
            times, notes, stats, free_times, free_sung = vocal_truth(data)
            song = dict(name=entry["name"], data=data, times=times, notes=notes, free_times=free_times,
                        free_sung=free_sung, harmony=harmony_at(data, free_times), first=entry["firstLine"])
            rows = json.loads((Path(args.cache) / digest[:16] / "production-evidence.json").read_text(encoding="utf-8"))
            row["seeded"] = evaluate(song, seeded_combos(rows, float(data["duration"]), combo_index[ext]))
    report = {"api": API, "holdSeconds": HOLD_SECONDS, "songs": results}
    (out / "reccobeats.json").write_text(json.dumps(report, ensure_ascii=False, indent=1), encoding="utf-8")

    n = len(results)
    matched = [r for r in results if r["match"]]
    with_key = [r for r in results if r.get("external")]
    lat = [sum(r["searchLatencyMs"]) + (r["match"]["featuresLatencyMs"] if r["match"] else 0) for r in results]
    print(f"\nsongs {n}; matched {len(matched)} (artist match {sum(r['match']['artistMatch'] for r in matched)}, "
          f"duration match {sum(r['match']['durationMatch'] for r in matched)}); key returned {len(with_key)}")
    print(f"same pitch-class set as offline best: {sum(r['sameSetAsOracle'] for r in with_key)}/{len(with_key)}; "
          f"same tonic+mode: {sum(r['sameTonicAsOracle'] for r in with_key)}/{len(with_key)}")
    print(f"cold lookup latency ms (searches + features): p50 {np.percentile(lat, 50):.0f}, p95 {np.percentile(lat, 95):.0f}")
    by = {e["name"]: e["strategies"] for e in holdout["songs"]}
    mean = lambda xs: float(np.mean([x for x in xs if x is not None])) if xs else float("nan")
    print("\nOn songs with an external key (seeded vs. production-only vs. S0 vs. second-play cache vs. oracle):")
    for label, get_row in (("external seed + analysis", lambda r: r["seeded"]),
                           ("production analysis only", lambda r: by[r["name"]]["PROD-majmin"]),
                           ("S0 current libkeyfinder", lambda r: by[r["name"]]["S0"]),
                           ("second play (own cache)", lambda r: by[r["name"]]["PROD-majmin-cached"]),
                           ("offline best fixed", lambda r: by[r["name"]]["S5-majmin"])):
        R = [get_row(r) for r in with_key]
        print(f"  {label:26s} firstLineCorrect {mean([x['firstLine']['outputCorrect'] for x in R]):.3f} "
              f"firstMis {mean([x['firstLine']['mispull'] for x in R]):.3f} wholeCorrect {mean([x['whole']['outputCorrect'] for x in R]):.3f} "
              f"mispull {mean([x['whole']['mispull'] for x in R]):.3f}")


if __name__ == "__main__":
    main()
