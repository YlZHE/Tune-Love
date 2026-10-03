"""Chromatic-when-uncertain gate on top of the production Major/Minor matcher (chromatic-gate-01).

The Rust implementation must follow exactly this definition.

  weights   = matcher.normalized(evidence, alpha)   (evidence + alpha/12) / (seconds + alpha)
  S         = the matcher's current set (hysteresis rule, first evidence commits)
  uncovered = the gate statistic (`statistic`, default "total"):
    total  sum of weights over pitch classes outside S              (chromatic-gate-01)
    top    max of weights over pitch classes outside S              (chromatic-gate-02)
    ratio  top / min of weights over pitch classes inside S         (chromatic-gate-02)
  state `chromatic` starts True:
    chromatic and seconds >= MIN_SECONDS and uncovered < EXIT: hold += step; hold >= EXIT_HOLD_SECONDS -> False
    chromatic otherwise: hold = 0
    not chromatic and uncovered > ENTER: chromatic = True, hold = 0
  output combo = Chromatic if chromatic else S

`seconds` is the accumulated evidence time (evidence.sum()); `step_seconds` is the evidence time
added by this step (0 for a silent step), so a silent step neither advances `seconds` nor the hold.
"""
import dataclasses
from typing import NamedTuple

import numpy as np

import matcher
from run_holdout import FROZEN

GAMMA = FROZEN["S1"].gamma
PRODUCTION_PARAMS = dataclasses.replace(FROZEN["S1"], family="majmin")  # what production runs today
STEP = 1.0


STATISTICS = ("total", "top", "ratio")


class GateParams(NamedTuple):
    enter: float
    exit: float
    exit_hold: float
    min_seconds: float
    statistic: str = "total"


def gate_statistic(name, weights, set_index):
    weights = np.asarray(weights)
    mask = matcher.MASKS[set_index]
    outside = weights[~mask]
    if name == "total":
        return float(outside.sum())
    top = float(outside.max()) if len(outside) else 0.0
    if name == "top":
        return top
    return top / float(weights[mask].min())  # name == "ratio"; the prior keeps the in-set minimum above 0


class Gate:
    def __init__(self, enter, exit, exit_hold, min_seconds, statistic="total"):
        if statistic not in STATISTICS:
            raise ValueError(f"unknown gate statistic {statistic!r}")
        self.params = GateParams(enter, exit, exit_hold, min_seconds, statistic)
        self.chromatic = True
        self.hold = 0.0
        self.seconds = 0.0
        self.set_index = matcher.CHROMATIC
        self.uncovered = 0.0

    def update(self, weights, set_index, step_seconds):
        """Advance one step; returns True when the output should be Chromatic."""
        p = self.params
        self.seconds += step_seconds
        self.set_index = set_index
        self.uncovered = gate_statistic(p.statistic, weights, set_index)
        if self.chromatic:
            if self.seconds >= p.min_seconds and self.uncovered < p.exit:
                self.hold += step_seconds
                if self.hold >= p.exit_hold:
                    self.chromatic = False
            else:
                self.hold = 0.0
        elif self.uncovered > p.enter:
            self.chromatic = True
            self.hold = 0.0
        return self.chromatic

    def combo(self, chromatic_index=matcher.CHROMATIC):
        return chromatic_index if self.chromatic else self.set_index


def production_chromatic(rows, duration, gate_params, return_sets=False):
    """Mirror replay_production.production_combos (majmin matcher) and gate its output per step.

    Returns combos[0..steps] (index 0 = before playback, Chromatic); with return_sets=True also the
    underlying Major/Minor set per step (what production writes today).
    """
    steps = int(np.ceil(duration / STEP))
    tracker = matcher.Tracker(PRODUCTION_PARAMS)
    gate = Gate(*gate_params)
    evidence = np.zeros(12)
    combos = np.full(steps + 1, matcher.CHROMATIC)
    sets = np.full(steps + 1, matcher.CHROMATIC)
    by_second = {r["second"]: r["evidence"] for r in rows if r["evidence"]}
    for k in range(1, steps + 1):
        added = 0.0
        ev = by_second.get(k)
        if ev and ev["seconds"] > 0 and ev["rms"] >= 1e-3:
            c = np.array(ev["chroma"], dtype=float)
            peak = c.max()
            if peak > 0:
                shaped = (c / peak) ** GAMMA
                evidence += shaped / shaped.sum() * ev["seconds"]
                added = float(ev["seconds"])
        s = tracker.update(evidence)
        gate.update(matcher.normalized(evidence, PRODUCTION_PARAMS.alpha), s, added)
        combos[k], sets[k] = gate.combo(), s
    return (combos, sets) if return_sets else combos
