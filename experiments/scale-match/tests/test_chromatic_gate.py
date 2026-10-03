import sys
import unittest
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import matcher  # noqa: E402
from chromatic_gate import Gate, GateParams, production_chromatic  # noqa: E402

C_MAJOR = matcher.COMBOS.index((0, "Major"))
PARAMS = GateParams(enter=0.10, exit=0.05, exit_hold=3.0, min_seconds=3.0)  # test values, not the frozen ones


def weights(uncovered, set_index=C_MAJOR):
    """Weights with `uncovered` spread evenly over pitch classes outside the set."""
    inside = matcher.MASKS[set_index]
    w = np.where(inside, (1.0 - uncovered) / inside.sum(), uncovered / (~inside).sum())
    return w


def run(gate, seq, set_index=C_MAJOR):
    """seq: iterable of uncovered values, one second each; returns the chromatic flag after every step."""
    return [gate.update(weights(u, set_index), set_index, 1.0) for u in seq]


class GateTests(unittest.TestCase):
    def test_starts_chromatic_until_min_seconds(self):
        gate = Gate(*PARAMS)
        out = run(gate, [0.0] * 8)  # only C-major evidence
        self.assertTrue(all(out[:2]))  # seconds 1, 2 < MIN_SECONDS
        self.assertTrue(out[3])  # qualifying has started at second 3, hold not yet satisfied
        self.assertFalse(out[5])  # MIN_SECONDS + EXIT_HOLD_SECONDS reached
        self.assertFalse(out[-1])

    def test_d_and_dflat_both_strong_enters_chromatic(self):
        gate = Gate(*PARAMS)
        run(gate, [0.0] * 8)
        self.assertFalse(gate.chromatic)
        f_minor = matcher.COMBOS.index((5, "Minor"))  # F G Ab Bb C Db Eb
        w = np.zeros(12)
        for pc in (5, 7, 8, 10, 0, 1, 3):
            w[pc] = 0.10
        w[2] = 0.15  # D natural, as sung in a Dorian line
        w[1] += 0.15  # Db, as in the F minor scale
        w /= w.sum()
        self.assertGreater(w[~matcher.MASKS[f_minor]].sum(), PARAMS.enter)
        self.assertTrue(gate.update(w, f_minor, 1.0))
        self.assertEqual(gate.set_index, f_minor)  # candidate set is still reported
        self.assertEqual(gate.combo(matcher.CHROMATIC), matcher.CHROMATIC)

    def test_oscillating_between_lines_holds_state(self):
        gate = Gate(*PARAMS)
        run(gate, [0.0] * 8)
        self.assertFalse(gate.chromatic)
        out = run(gate, [0.08, 0.06, 0.09, 0.07] * 5)  # between EXIT and ENTER
        self.assertFalse(any(out))
        run(gate, [0.20])
        self.assertTrue(gate.chromatic)
        out = run(gate, [0.08, 0.06, 0.09, 0.07] * 5)  # still above EXIT
        self.assertTrue(all(out))

    def test_exit_requires_hold(self):
        gate = Gate(*PARAMS)
        run(gate, [0.0] * 8)
        run(gate, [0.20])
        self.assertTrue(gate.chromatic)
        out = run(gate, [0.01, 0.01, 0.20, 0.01, 0.01])  # a blip above EXIT resets the hold
        self.assertTrue(all(out))
        run(gate, [0.20])  # reset the hold again (two good seconds were banked)
        out = run(gate, [0.01, 0.01, 0.01])
        self.assertEqual(out, [True, True, False])  # EXIT_HOLD_SECONDS = 3

    def test_no_enter_exactly_at_threshold(self):
        gate = Gate(*PARAMS)
        run(gate, [0.0] * 8)
        self.assertFalse(run(gate, [PARAMS.enter])[0])  # strict >
        self.assertTrue(run(gate, [PARAMS.enter + 1e-6])[0])


class ProductionChromaticTests(unittest.TestCase):
    @staticmethod
    def rows(n, pcs):
        c = [1.0 if pc in pcs else 0.0 for pc in range(12)]
        return [{"second": s, "evidence": {"seconds": 1.0, "rms": 0.1, "chroma": c}} for s in range(1, n + 1)]

    def test_chromatic_first_then_set(self):
        rows = self.rows(40, (0, 2, 4, 5, 7, 9, 11))
        gp = GateParams(enter=0.10, exit=0.09, exit_hold=3.0, min_seconds=3.0)
        combos = production_chromatic(rows, 40.0, gp)
        self.assertEqual(len(combos), 41)
        self.assertEqual(combos[0], matcher.CHROMATIC)
        self.assertEqual(combos[1], matcher.CHROMATIC)
        self.assertEqual(matcher.COMBOS[combos[-1]][1], "Major")
        # leaving Chromatic happens exactly once, never re-entered on clean evidence
        self.assertEqual(int((np.diff((combos == matcher.CHROMATIC).astype(int)) == -1).sum()), 1)
        self.assertFalse((np.diff((combos == matcher.CHROMATIC).astype(int)) == 1).any())

    def test_silence_stays_chromatic(self):
        rows = [{"second": s, "evidence": {"seconds": 1.0, "rms": 0.0, "chroma": [1.0] * 12}} for s in range(1, 20)]
        combos = production_chromatic(rows, 19.0, PARAMS)
        self.assertTrue((combos == matcher.CHROMATIC).all())


if __name__ == "__main__":
    unittest.main()
