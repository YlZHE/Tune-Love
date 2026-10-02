import sys
import unittest
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import matcher  # noqa: E402
import recent_vocal  # noqa: E402
from scales import mask  # noqa: E402

PARAMS = matcher.Params(w_extra=1.0, tau=0.04, alpha=2.0, margin=0.01, gamma=4.0, family="majmin")


def set_evidence(pcs, seconds):
    ev = np.zeros(12)
    ev[list(pcs)] = seconds / len(pcs)
    return ev


class RecentVocalTests(unittest.TestCase):
    def test_zero_weight_matches_matcher_tracker(self):
        rng = np.random.default_rng(0)
        base = np.cumsum(rng.random((120, 12)) ** 3, axis=0)
        recent = rng.random((120, 12)) * 5
        ref, new = matcher.Tracker(PARAMS), recent_vocal.RecentVocalTracker(PARAMS, 0.0)
        for k in range(120):
            self.assertEqual(ref.update(base[k]), new.update(base[k], recent[k]))

    def test_recent_out_of_set_notes_move_the_choice(self):
        c_major = [0, 2, 4, 5, 7, 9, 11]
        base = set_evidence(c_major, 120.0)                       # long C-major history
        recent = set_evidence([6, 7, 11, 2], 6.0)                 # singer now leans on F#
        plain = recent_vocal.RecentVocalTracker(PARAMS, 0.0)
        pushed = recent_vocal.RecentVocalTracker(PARAMS, 4.0)
        for _ in range(3):
            a, b = plain.update(base, recent), pushed.update(base, recent)
        self.assertEqual(matcher.COMBOS[a], (0, "Major"))
        self.assertTrue(mask(*matcher.COMBOS[b])[6])            # chosen set now contains F#
        self.assertNotEqual(matcher.COMBOS[b][1], "Chromatic")

    def test_below_min_seconds_has_no_effect(self):
        base = set_evidence([0, 2, 4, 5, 7, 9, 11], 60.0)
        recent = set_evidence([6], recent_vocal.MIN_SECONDS * 0.5)
        a = recent_vocal.RecentVocalTracker(PARAMS, 0.0)
        b = recent_vocal.RecentVocalTracker(PARAMS, 4.0)
        for _ in range(3):
            self.assertEqual(a.update(base, recent), b.update(base, recent))


class R2Tests(unittest.TestCase):
    C, G, F = [0, 2, 4, 5, 7, 9, 11], [7, 9, 11, 0, 2, 4, 6], [5, 7, 9, 10, 0, 2, 4]

    def run_steps(self, tracker, plan):
        """plan: list of (base pcs, recent-vocal pcs, recent-mix pcs) per step; returns chosen combos."""
        return [matcher.COMBOS[tracker.update(set_evidence(b, 60.0), set_evidence(v, 8.0),
                                              set_evidence(m, 8.0))] for b, v, m in plan]

    def test_defaults_equal_r(self):
        rng = np.random.default_rng(1)
        base = np.cumsum(rng.random((80, 12)) ** 3, axis=0)
        recent, mix = rng.random((80, 12)) * 5, rng.random((80, 12)) * 5
        r = recent_vocal.RecentVocalTracker(PARAMS, 2.0)
        r2 = recent_vocal.RecentVocalTracker(PARAMS, 2.0, dwell=0, tol=None)
        for k in range(80):
            self.assertEqual(r.update(base[k], recent[k]), r2.update(base[k], recent[k], mix[k]))

    def test_first_commit_is_not_delayed_and_dwell_blocks_flip_back(self):
        plan = [(self.C, self.C, self.C)] + [(self.C, self.G, self.G)] * 3 + [(self.C, self.F, self.F)] * 3
        free = self.run_steps(recent_vocal.RecentVocalTracker(PARAMS, 4.0), plan)
        held = self.run_steps(recent_vocal.RecentVocalTracker(PARAMS, 4.0, dwell=10), plan)
        self.assertEqual(held[0], free[0])                         # first commit at once
        self.assertEqual(free[1][0], 7)                             # R alone follows G...
        self.assertEqual(free[4][0], 5)                             # ...then F
        self.assertEqual(held[1][0], 7)                             # first real switch allowed
        self.assertTrue(all(c[0] == 7 for c in held[1:]))          # no further switch within 10 s

    def test_gate_blocks_switch_the_recent_mix_does_not_support(self):
        plan = [(self.C, self.C, self.C)] + [(self.C, self.G, self.C)] * 3  # singer leans F#, song stays C
        gated = self.run_steps(recent_vocal.RecentVocalTracker(PARAMS, 4.0, tol=0.0), plan)
        self.assertTrue(all(c == (0, "Major") for c in gated))
        plan = [(self.C, self.C, self.C)] + [(self.C, self.G, self.G)] * 3  # song moves to G as well
        moved = self.run_steps(recent_vocal.RecentVocalTracker(PARAMS, 4.0, tol=0.0), plan)
        self.assertEqual(moved[-1][0], 7)


class R3Tests(unittest.TestCase):
    def test_onset_step(self):
        times = np.arange(300) * 0.01 + 5.5                  # 3 s of notes starting at 5.5 s
        notes = np.full(300, 66)
        self.assertEqual(recent_vocal.onset_step(times, notes, 20.0, 0), 0)
        self.assertEqual(recent_vocal.onset_step(times, notes, 20.0, 5), 8 + 5)   # 1.5 s visible at 7, 2.5 s at 8
        self.assertGreater(recent_vocal.onset_step(times[:100], notes[:100], 20.0, 5), 20)  # never: after the last step

    def test_r_term_waits_for_active_from(self):
        c_major = [0, 2, 4, 5, 7, 9, 11]
        base, recent = set_evidence(c_major, 120.0), set_evidence([6, 7, 11, 2], 6.0)
        early = recent_vocal.RecentVocalTracker(PARAMS, 4.0)
        late = recent_vocal.RecentVocalTracker(PARAMS, 4.0, active_from=4)
        got = [(matcher.COMBOS[early.update(base, recent)], matcher.COMBOS[late.update(base, recent)]) for _ in range(5)]
        self.assertTrue(mask(*got[1][0])[6])                 # without delay: F# set already at step 2
        self.assertEqual([g[1] for g in got[:3]], [(0, "Major")] * 3)   # delayed: C major until step 4
        self.assertTrue(mask(*got[4][1])[6])


class R4Tests(unittest.TestCase):
    def test_early_switches_are_free_then_dwell_applies(self):
        C, G, F = R2Tests.C, R2Tests.G, R2Tests.F
        plan = ([(C, C, C)] + [(G, G, G)] * 2 + [(F, F, F)] * 2      # early convergence: C -> G -> F
                + [(G, G, G)] * 2 + [(C, C, C)] * 2)                    # later: G, then C again
        run = lambda t: R2Tests.run_steps(None, t, plan)
        strict = run(recent_vocal.RecentVocalTracker(PARAMS, 0.0, dwell=10))
        delayed = run(recent_vocal.RecentVocalTracker(PARAMS, 0.0, dwell=10, dwell_from=5))
        self.assertEqual([c[0] for c in strict[:5]], [0, 7, 7, 7, 7])      # dwell blocks the 2nd early fix
        self.assertEqual([c[0] for c in delayed[:5]], [0, 7, 7, 5, 5])     # free before step 5
        self.assertEqual(delayed[5][0], 7)                                 # step 6: first counted switch
        self.assertTrue(all(c[0] == 7 for c in delayed[5:]))               # then held for 10 s

    def test_zero_dwell_from_equals_r2(self):
        rng = np.random.default_rng(2)
        base = np.cumsum(rng.random((80, 12)) ** 3, axis=0)
        recent, mix = rng.random((80, 12)) * 5, rng.random((80, 12)) * 5
        a = recent_vocal.RecentVocalTracker(PARAMS, 2.0, dwell=10, tol=0.05)
        b = recent_vocal.RecentVocalTracker(PARAMS, 2.0, dwell=10, tol=0.05, dwell_from=0)
        for k in range(80):
            self.assertEqual(a.update(base[k], recent[k], mix[k]), b.update(base[k], recent[k], mix[k]))


if __name__ == "__main__":
    unittest.main()
