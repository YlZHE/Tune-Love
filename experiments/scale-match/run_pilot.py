"""Stage A pilot: compare Key/Scale strategies by whether the original singer's notes survive snapping.

Strategies
  S0  current libkeyfinder live rule (6-8 s windows, 3/5 votes) -> Key + Major/Minor; Chromatic until confirmed
  S1  matcher on mix chroma (what production can see)
  S2  matcher on accompaniment-stem chroma
  S3  matcher on mix chroma + original-vocal note evidence (needs real-time separation; reference only)
  S4  always Chromatic
  S5  oracle: best fixed combo for the whole song from the vocal truth (non-causal upper bound)
"""
import argparse
import itertools
import json
import subprocess
import sys
from pathlib import Path

import numpy as np

import lrc
import matcher
from features import decode, load_song
from metrics import combined, consonance, note_frames, success_table, summarize
from scales import label

STEP = 1.0
SOURCES = ("S1", "S2", "S3", "S3g")  # S3g: vocal evidence from real-time StemgenRT stem
TABLE = success_table(matcher.MASKS)
PROBE = Path(__file__).resolve().parents[2] / "artifacts/key-engine-evaluation/bin/default-v2/key_window_probe.exe"

GRID = {
    "w_extra": [0.25, 0.5, 1.0],
    "tau": [0.02, 0.04],
    "alpha": [2.0, 5.0, 10.0],
    "margin": [0.01, 0.03],
    "gamma": [1.0, 2.0, 4.0],
    "halflife": [0.0, 60.0],
    "family": ["all", "diatonic"],
}
DEFAULT = matcher.Params()


# ---------- truth ----------

def vocal_truth(data):
    f0, period, rms = data["f0"], data["period"], data["f0_rms"]
    gate = np.percentile(rms, 99) * 10 ** (-35 / 20)
    voiced = (period >= 0.5) & (rms >= gate) & (f0 > 0)
    midi = 69 + 12 * np.log2(np.maximum(f0, 1e-6) / 440.0)
    hop = float(data["f0_t"][1] - data["f0_t"][0])
    evaluable, note, stats = note_frames(data["f0_t"], midi, voiced, hop)
    free = voiced & ~evaluable                      # glides, rap, speech-like: judged by consonance
    sung = midi - stats["tuningOffsetCents"] / 100.0
    stats["unstableSeconds"] = round(float(free.sum()) * hop, 2)
    return data["f0_t"][evaluable], note[evaluable], stats, data["f0_t"][free], sung[free]


def harmony_at(data, times, threshold=0.35, half_window=0.5):
    """Pitch classes prominent in the separated accompaniment within +-half_window s (judge only)."""
    t, c = data["acc_t"], data["acc_c"] * (data["acc_rms"] >= 1e-3)[:, None]
    csum = np.vstack([np.zeros((1, 12)), np.cumsum(c, axis=0)])
    lo = np.searchsorted(t, times - half_window)
    hi = np.searchsorted(t, times + half_window, side="right")
    avg = (csum[hi] - csum[lo]) / np.maximum(hi - lo, 1)[:, None]
    peak = avg.max(axis=1, keepdims=True)
    return (avg >= threshold * peak) & (peak > 0)


def stemgen_vocal_notes(cache_dir, stem="stemgenrt"):
    """Vocal note evidence from a separated vocal stem (same F0/note pipeline as truth).

    stem: "stemgenrt" (real-time playback path) or "bytesep" (light background separator).
    """
    feats = cache_dir / f"{stem}-f0.npz"
    if not feats.exists():
        src = cache_dir / f"{stem}-vocals.npy"
        if not src.exists():
            return None, None
        from features import SEP_RATE, vocal_f0
        vocals = np.load(src).astype(np.float32)
        f0_t, f0, period, f0_rms = vocal_f0(vocals.mean(0), SEP_RATE)
        np.savez_compressed(feats, f0_t=f0_t, f0=f0, period=period, f0_rms=f0_rms)
    data = dict(np.load(feats))
    times, notes, _, _, _ = vocal_truth(data)
    return times, notes


# ---------- evidence ----------

def chroma_frames(times, c, rms, gamma):
    peak = c.max(axis=1, keepdims=True)
    shaped = np.where(peak > 0, (c / np.maximum(peak, 1e-12)) ** gamma, 0.0)
    shaped = shaped / np.maximum(shaped.sum(axis=1, keepdims=True), 1e-12)
    active = rms >= 1e-3                       # ~ -60 dBFS: skip silence
    hop = float(times[1] - times[0])
    return times, shaped * active[:, None] * hop


def vocal_frames(times, notes, hop=0.01):
    ev = np.zeros((len(times), 12))
    ev[np.arange(len(times)), notes % 12] = hop
    return times, ev


def stepped(frames_list, duration, halflife):
    """Evidence accumulated up to each step time t = 1, 2, ... (causal)."""
    steps = int(np.ceil(duration / STEP))
    per_step = np.zeros((steps + 1, 12))
    for times, ev in frames_list:
        idx = np.minimum((times // STEP).astype(int) + 1, steps)  # frame in [k, k+1) visible at t = k+1
        np.add.at(per_step, idx, ev)
    decay = 0.5 ** (STEP / halflife) if halflife > 0 else 1.0
    out = np.zeros_like(per_step)
    for k in range(1, steps + 1):
        out[k] = out[k - 1] * decay + per_step[k]
    return out  # out[k] = evidence visible at time k*STEP


def track(evidence, params):
    tracker = matcher.Tracker(params)
    combos = np.full(len(evidence), matcher.CHROMATIC)
    for k in range(1, len(evidence)):
        combos[k] = tracker.update(evidence[k])
    return combos  # combos[k] active during [k, k+1)


# ---------- S0 ----------

def s0_candidates(path, cache_dir):
    cached = cache_dir / "s0-probe.json"
    if cached.exists():
        return {int(k): v for k, v in json.loads(cached.read_text()).items()}
    pcm = decode(path, 48_000, 2).T.reshape(-1).astype("<f4")
    frames = len(pcm) // 2
    cands, start = {}, 0
    while start * 48_000 + 6 * 48_000 <= frames:
        chunk = pcm[start * 96_000:(start + 12) * 96_000]   # frozen probe accepts --end 6..12
        end = min(12, len(chunk) // 96_000)
        out = subprocess.run([str(PROBE), "--mode", "batch", "--end", str(end)], input=chunk.tobytes(),
                             capture_output=True, check=True)
        for row in json.loads(out.stdout)["rows"]:
            if start == 0 or row["second"] >= 8:
                cands[start + row["second"]] = row["candidate"]
        start += 5  # next chunk's first full 8 s window ends at previous end + 1
    cached.write_text(json.dumps(cands))
    return cands


def s0_track(cands, steps):
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
            if streak >= (5 if confirmed else 3):
                confirmed, pending, streak = key, None, 0
        combos[k] = combo_of[confirmed] if confirmed else matcher.CHROMATIC
    return combos


# ---------- evaluation ----------

def oracle_combo(song):
    """Fixed combo with the best combined score over the whole song (non-causal upper bound)."""
    steps = int(np.ceil(float(song["data"]["duration"]) / STEP))
    scores = [evaluate(song, np.full(steps + 1, i))["combined"] or 0.0 for i in range(len(matcher.COMBOS))]
    return int(np.argmax(scores))


def active_at(times, combos):
    return combos[np.minimum((times // STEP).astype(int), len(combos) - 1)]


def evaluate(song, combos):
    times, notes, first = song["times"], song["notes"], song["first"]
    steps = len(combos) - 1
    active = active_at(times, combos)
    pcs = notes % 12
    in_first = (times >= first[0]) & (times < first[1])
    free_active = active_at(song["free_times"], combos)
    free_first = (song["free_times"] >= first[0]) & (song["free_times"] < first[1])
    start_combo = int(combos[min(int(first[0] // STEP), steps)])
    first_pcs = np.unique(pcs[in_first])
    whole = summarize(pcs, active, TABLE)
    free = consonance(song["free_sung"], free_active, matcher.MASKS, song["harmony"])
    first_stable = summarize(pcs[in_first], active[in_first], TABLE)
    first_free = consonance(song["free_sung"][free_first], free_active[free_first], matcher.MASKS,
                            song["harmony"][free_first])
    return {
        "whole": whole,
        "unstable": free,
        "combined": combined(whole, free),
        "firstLine": first_stable,
        "firstLineUnstable": first_free,
        "firstLineCombined": combined(first_stable, first_free),
        "atFirstLine": label(*matcher.COMBOS[start_combo]),
        "narrowedBeforeFirstLine": start_combo != matcher.CHROMATIC,
        "firstLineCovered": bool(matcher.MASKS[start_combo][first_pcs].all()) if len(first_pcs) else None,
        "switches": int((np.diff(combos) != 0).sum()),
        "finalCombo": label(*matcher.COMBOS[int(combos[-1])]),
    }


def matcher_combos(song, params, source):
    mix = chroma_frames(song["data"]["mix_t"], song["data"]["mix_c"], song["data"]["mix_rms"], params.gamma)
    if source == "S1":
        frames = [mix]
    elif source == "S2":
        frames = [chroma_frames(song["data"]["acc_t"], song["data"]["acc_c"], song["data"]["acc_rms"], params.gamma)]
    elif source == "S3g":
        frames = [mix, vocal_frames(song["gen_times"], song["gen_notes"])]
    elif source == "S3b":
        frames = [mix, vocal_frames(song["bs_times"], song["bs_notes"])]
    else:
        frames = [mix, vocal_frames(song["times"], song["notes"])]
    evidence = stepped(frames, float(song["data"]["duration"]), params.halflife)
    return track(evidence, params)


def objective(result):
    return 1.0 - result["combined"]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--songs", required=True)
    parser.add_argument("--cache", required=True)
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    sys.stdout.reconfigure(encoding="utf-8")
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=False)  # exclusive: never overwrite evidence

    songs = []
    for path in sorted(Path(args.songs).glob("*.mp3")):
        digest, data = load_song(path, args.cache)
        lines = lrc.parse(path.with_suffix(".lrc").read_text(encoding="utf-8-sig"))
        times, notes, stats, free_times, free_sung = vocal_truth(data)
        cache_dir = Path(args.cache) / digest[:16]
        gen_times, gen_notes = stemgen_vocal_notes(cache_dir)
        songs.append(dict(gen_times=gen_times, gen_notes=gen_notes,name=path.name, sha256=digest, data=data, times=times, notes=notes,
                          free_times=free_times, free_sung=free_sung, harmony=harmony_at(data, free_times),
                          stats=stats, first=lrc.first_line(lines), cands=s0_candidates(path, cache_dir)))

    grid = [matcher.Params(**dict(zip(GRID, values))) for values in itertools.product(*GRID.values())]
    report = {"schemaVersion": 1, "step": STEP, "default": DEFAULT.__dict__, "grid": GRID, "songs": []}
    grid_scores = {s: np.zeros((len(grid), len(songs))) for s in SOURCES}
    for j, song in enumerate(songs):
        steps = int(np.ceil(float(song["data"]["duration"]) / STEP))
        entry = {"name": song["name"], "sha256": song["sha256"], "duration": round(float(song["data"]["duration"]), 1),
                 "firstLine": song["first"], "truth": song["stats"], "strategies": {}}
        entry["strategies"]["S0"] = evaluate(song, s0_track(song["cands"], steps))
        entry["strategies"]["S4"] = evaluate(song, np.full(steps + 1, matcher.CHROMATIC))
        entry["strategies"]["S5"] = evaluate(song, np.full(steps + 1, oracle_combo(song)))
        for s in SOURCES:
            entry["strategies"][s + "-default"] = evaluate(song, matcher_combos(song, DEFAULT, s))
            for g, params in enumerate(grid):
                grid_scores[s][g, j] = objective(evaluate(song, matcher_combos(song, params, s)))
        report["songs"].append(entry)
        print(f"done {song['name']}", flush=True)

    # Leave-one-out: choose params on the other songs, evaluate on the held-out one.
    for s in SOURCES:
        for j, song in enumerate(songs):
            others = [i for i in range(len(songs)) if i != j]
            g = int(np.argmin(grid_scores[s][:, others].mean(axis=1)))
            result = evaluate(song, matcher_combos(song, grid[g], s))
            result["params"] = grid[g].__dict__
            report["songs"][j]["strategies"][s + "-loo"] = result
        best = int(np.argmin(grid_scores[s].mean(axis=1)))
        report[f"{s}-bestOnAll"] = {"params": grid[best].__dict__, "meanObjective": float(grid_scores[s][best].mean())}

    (out / "pilot.json").write_text(json.dumps(report, ensure_ascii=False, indent=1), encoding="utf-8")
    print_summary(report)


def print_summary(report):
    names = ["S0", "S1-loo", "S2-loo", "S3-loo", "S3g-default", "S3g-loo", "S4", "S5"]
    print("\nstrategy      mispull  outputCorrect  unstableConsonant  combined  firstLineCombined  narrowed  switches")
    for n in names:
        rows = [s["strategies"][n] for s in report["songs"]]
        mean = lambda xs: np.mean([x for x in xs if x is not None]) if any(x is not None for x in xs) else float("nan")
        print(f"{n:12s}  {mean([r['whole']['mispull'] for r in rows]):7.3f}  "
              f"{mean([r['whole']['outputCorrect'] for r in rows]):13.3f}  "
              f"{mean([r['unstable']['consonant'] for r in rows]):17.3f}  "
              f"{mean([r['combined'] for r in rows]):8.3f}  "
              f"{mean([r['firstLineCombined'] for r in rows]):17.3f}  "
              f"{sum(r['narrowedBeforeFirstLine'] for r in rows):>8d}  "
              f"{mean([r['switches'] for r in rows]):8.1f}")


if __name__ == "__main__":
    main()
