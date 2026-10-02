"""Pick the Auto-Tune Key/Scale combo that best fits accumulated pitch-class evidence.

cost = w_miss * (evidence weight outside the set)
     + w_extra * (sum over in-set pitch classes of their evidence shortfall below tau) / 12

A uniform prior of `alpha` seconds is mixed into the evidence, so with little
evidence every 7-note set "misses" ~5/12 and Chromatic (zero miss) wins. As evidence
accumulates the set narrows. Switching uses a cost margin (hysteresis).
"""
from dataclasses import dataclass

import numpy as np

from scales import all_combos

COMBOS, MASKS = all_combos()
CHROMATIC = COMBOS.index((0, "Chromatic"))
FAMILIES = {
    "all": np.ones(len(COMBOS), dtype=bool),
    "diatonic": np.array([s in ("Chromatic", "Major", "Minor") for _, s in COMBOS]),
    # User decision 2026-10-02: never Chromatic (it barely corrects in practice).
    "majmin": np.array([s in ("Major", "Minor") for _, s in COMBOS]),
}


@dataclass(frozen=True)
class Params:
    w_extra: float = 0.5   # relative to w_miss = 1
    tau: float = 0.04      # in-set pitch class weight considered "used"
    alpha: float = 5.0     # prior strength, in seconds of evidence
    margin: float = 0.02   # required cost improvement to switch
    gamma: float = 2.0     # chroma sharpening exponent (chroma evidence only)
    halflife: float = 0.0  # evidence decay half-life in seconds; 0 = no decay
    family: str = "all"    # "all" or "diatonic" (Major/Minor + Chromatic only)


def normalized(evidence, alpha):
    total = float(evidence.sum())
    return (evidence + alpha / 12.0) / (total + alpha)


def costs(weights, params):
    miss = (weights[None, :] * ~MASKS).sum(axis=1)
    shortfall = np.clip(1.0 - weights / params.tau, 0.0, 1.0)
    extra = (shortfall[None, :] * MASKS).sum(axis=1) / 12.0
    return np.where(FAMILIES[params.family], miss + params.w_extra * extra, np.inf)


def best(weights, params):
    c = costs(weights, params)
    return int(np.argmin(c)), c  # argmin keeps the first (preferred) of equal costs


class Tracker:
    """Causal, stateful combo choice with hysteresis."""

    def __init__(self, params):
        self.params = params
        self.current = CHROMATIC

    def update(self, evidence):
        weights = normalized(evidence, self.params.alpha)
        idx, c = best(weights, self.params)
        if not np.isfinite(c[self.current]):
            # Current choice is outside the family (the initial "undecided" Chromatic
            # sentinel under majmin): commit to the best allowed combo as soon as any
            # evidence exists, without hysteresis.
            if evidence.sum() > 0:
                self.current = idx
        elif idx != self.current and c[idx] < c[self.current] - self.params.margin:
            self.current = idx
        return self.current
