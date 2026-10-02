"""Mechanism R: penalize Key/Scale sets that exclude notes the original singer has been singing recently.

The whole-song mix (with vocals) stays the main, cumulative evidence (as in matcher.Tracker). On top,
original-vocal note evidence decayed with half-life h ("recent vocal") adds

    w * (recent vocal evidence outside the set) / (recent vocal evidence)

to every combo's cost once the recent vocal evidence reaches min_seconds. With w = 0 this is exactly
matcher.Tracker. Preregistration: work/scale-match-20261002/preregistration.md, section frontend-b-04.

R2 (section frontend-b-05) adds two constraints on every switch after the first commit:
  dwell  no switch within `dwell` seconds (steps) of the previous switch;
  tol    accompaniment compatibility: the candidate's share of RECENT MIX evidence (whole song with vocals,
         decayed with the same half-life) outside its set may exceed the current set's by at most tol.
dwell = 0 and tol = None give R.

R3 (section frontend-b-06) adds an onset delay: the R term only applies from step `active_from`
(= first step where the cumulative original-vocal evidence reaches MIN_SECONDS, plus N seconds).
active_from = 0 gives R2.

R4 (section frontend-b-07) delays the dwell rule: before step `dwell_from` (cumulative MIX evidence first
reaches X seconds) switches are neither blocked by nor start a dwell period. dwell_from = 0 gives R2.
"""
import numpy as np

import matcher
from run_pilot import chroma_frames, stepped, vocal_frames

MIN_SECONDS = 2.0


class RecentVocalTracker:
    def __init__(self, params, w, min_seconds=MIN_SECONDS, dwell=0, tol=None, active_from=0, dwell_from=0):
        self.params, self.w, self.min_seconds, self.dwell, self.tol = params, w, min_seconds, dwell, tol
        self.active_from, self.dwell_from, self.step = active_from, dwell_from, 0
        self.current = matcher.CHROMATIC
        self.since_switch = 10**9  # the first commit does not start a dwell period

    def compatible(self, idx, recent_mix):
        if self.tol is None or recent_mix is None or recent_mix.sum() <= 0:
            return True
        outside = (recent_mix[None, :] * ~matcher.MASKS[[idx, self.current]]).sum(axis=1) / recent_mix.sum()
        return outside[0] <= outside[1] + self.tol

    def update(self, base, recent, recent_mix=None):
        self.since_switch += 1
        self.step += 1
        c = matcher.costs(matcher.normalized(base, self.params.alpha), self.params)
        total = float(recent.sum())
        if self.w and total >= self.min_seconds and self.step >= self.active_from:
            c = c + self.w * (recent[None, :] * ~matcher.MASKS).sum(axis=1) / total
        idx = int(np.argmin(c))
        if not np.isfinite(c[self.current]):
            if base.sum() > 0:   # same "commit on first evidence" rule as matcher.Tracker under majmin
                self.current = idx
        elif (idx != self.current and c[idx] < c[self.current] - self.params.margin
              and (self.step < self.dwell_from or self.since_switch > self.dwell)
              and self.compatible(idx, recent_mix)):
            self.current = idx
            if self.step >= self.dwell_from:   # early (convergence) switches do not start a dwell period
                self.since_switch = 0
        return self.current


def streams(view, gen_times, gen_notes, duration, params, halflife):
    """Per-step cumulative evidence (mix + vocal, as S3g), decayed recent vocal and decayed recent mix."""
    mix = chroma_frames(view["mix_t"], view["mix_c"], view["mix_rms"], params.gamma)
    vocal = vocal_frames(gen_times, gen_notes)
    return (stepped([mix, vocal], duration, params.halflife), stepped([vocal], duration, halflife),
            stepped([mix], duration, halflife))


def onset_step(gen_times, gen_notes, duration, delay):
    """Step from which R may act: cumulative vocal evidence first >= MIN_SECONDS, plus `delay` s (causal)."""
    if not delay:
        return 0
    cumulative = stepped([vocal_frames(gen_times, gen_notes)], duration, 0.0).sum(axis=1)
    hit = np.flatnonzero(cumulative >= MIN_SECONDS)
    return int(hit[0]) + int(delay) if len(hit) else len(cumulative) + 1


def mix_seconds_step(view, duration, params, seconds):
    """First step at which the cumulative mix evidence reaches `seconds` (0 if seconds == 0)."""
    if not seconds:
        return 0
    mix = chroma_frames(view["mix_t"], view["mix_c"], view["mix_rms"], params.gamma)
    cumulative = stepped([mix], duration, 0.0).sum(axis=1)
    hit = np.flatnonzero(cumulative >= seconds)
    return int(hit[0]) if len(hit) else len(cumulative) + 1


def track(base, recent, params, w, dwell=0, tol=None, recent_mix=None, active_from=0, dwell_from=0):
    tracker = RecentVocalTracker(params, w, dwell=dwell, tol=tol, active_from=active_from, dwell_from=dwell_from)
    combos = np.full(len(base), matcher.CHROMATIC)
    for k in range(1, len(base)):
        combos[k] = tracker.update(base[k], recent[k], None if recent_mix is None else recent_mix[k])
    return combos  # combos[k] active during [k, k+1) s, as run_pilot.track
