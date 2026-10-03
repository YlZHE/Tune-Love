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


def settled(params):
    """A gate of the given params that has already left Chromatic on clean in-set evidence."""
    gate = Gate(*params)
    run(gate, [0.0] * 8)
    assert not gate.chromatic
    return gate


def leaky_weights():
    """Diffuse leakage: 1.5% on each of five out-of-set notes (7.5% in total, no single note stands out)."""
    inside = matcher.MASKS[C_MAJOR]
    w = np.where(inside, (1.0 - 0.075) / 7, 0.015)
    return w


def borrowed_weights():
    """One genuinely borrowed note: D-flat at 8% in a C major set, the other out-of-set notes near zero."""
    inside = matcher.MASKS[C_MAJOR]
    w = np.where(inside, 0.12, 0.0025)
    w[1] = 0.08
    return w / w.sum()


class TopOutStatisticTests(unittest.TestCase):
    TOP = GateParams(enter=0.05, exit=0.03, exit_hold=3.0, min_seconds=3.0, statistic="top")
    TOTAL = GateParams(enter=0.06, exit=0.04, exit_hold=3.0, min_seconds=3.0, statistic="total")

    def test_default_statistic_is_total(self):
        self.assertEqual(GateParams(0.1, 0.05, 3.0, 3.0).statistic, "total")

    def test_diffuse_leakage_does_not_trigger_top(self):
        gate = settled(self.TOP)
        for _ in range(10):
            self.assertFalse(gate.update(leaky_weights(), C_MAJOR, 1.0))
        self.assertAlmostEqual(gate.uncovered, 0.015)  # the single strongest out-of-set note

    def test_same_diffuse_leakage_does_trigger_total(self):
        gate = settled(self.TOTAL)
        self.assertTrue(gate.update(leaky_weights(), C_MAJOR, 1.0))  # 7.5% in total > ENTER

    def test_strong_borrowed_note_triggers_top(self):
        gate = settled(self.TOP)
        self.assertTrue(gate.update(borrowed_weights(), C_MAJOR, 1.0))
        self.assertGreater(gate.uncovered, self.TOP.enter)

    def test_diffuse_leakage_lets_a_chromatic_gate_exit(self):
        gate = Gate(*self.TOP)
        out = [gate.update(leaky_weights(), C_MAJOR, 1.0) for _ in range(8)]
        self.assertTrue(out[0] and out[1])
        self.assertFalse(out[-1])  # 1.5% < EXIT, so it leaves after MIN_SECONDS + hold

    def test_ratio_is_top_out_over_weakest_in_set_note(self):
        gate = Gate(1.5, 0.75, 3.0, 3.0, "ratio")
        w = np.where(matcher.MASKS[C_MAJOR], 0.12, 0.0)
        w[1] = 0.06   # strongest out-of-set note
        w[0] = 0.03   # weakest in-set note
        gate.update(w, C_MAJOR, 1.0)
        self.assertAlmostEqual(gate.uncovered, 2.0)

    def test_unknown_statistic_is_rejected(self):
        with self.assertRaises(ValueError):
            Gate(0.1, 0.05, 3.0, 3.0, "median")


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


class RustParityTests(unittest.TestCase):
    """The five sequences that src-tauri/src/key_detection/scale_match.rs hard-codes as its
    parity tests (parity_*), replayed through production_chromatic at the frozen gate
    parameters. The expected strings are the Rust tests' strings: 'C' = Chromatic."""

    FROZEN = GateParams(3.0, 1.5, 3.0, 3.0, "ratio")
    C_MAJOR_PCS = (0, 2, 4, 5, 7, 9, 11)
    G_MAJOR_PCS = (7, 9, 11, 0, 2, 4, 6)

    @staticmethod
    def levels(spec, rms=0.1):
        c = [0.0] * 12
        for pc, level in spec:
            c[pc] = level
        return {"seconds": 1.0, "rms": rms, "chroma": c}

    def flags(self, evidences):
        rows = [{"second": k, "evidence": ev} for k, ev in enumerate(evidences, 1)]
        combos = production_chromatic(rows, float(len(evidences)), self.FROZEN)
        return "".join("C" if c == matcher.CHROMATIC else "." for c in combos[1:])

    def flat(self, pcs):
        return self.levels([(pc, 1.0) for pc in pcs])

    D_AND_DFLAT = [(5, 1.0), (7, 1.0), (8, 1.0), (10, 1.0), (0, 1.0), (1, 1.0), (3, 0.8), (2, 1.0)]

    def test_clean_c_major(self):
        self.assertEqual(self.flags([self.flat(self.C_MAJOR_PCS)] * 30), "CCCC" + "." * 26)

    def test_d_and_dflat_strong_stays_chromatic(self):
        self.assertEqual(self.flags([self.levels(self.D_AND_DFLAT)] * 20), "C" * 20)

    def test_equal_weight_extra_note_leaves_chromatic(self):
        spec = [(pc, 1.0 if pc == 3 else level) for pc, level in self.D_AND_DFLAT]
        self.assertEqual(self.flags([self.levels(spec)] * 20), "CCCC" + "." * 16)

    def test_set_change_does_not_reenter_chromatic(self):
        seq = [self.flat(self.C_MAJOR_PCS)] * 12 + [self.flat(self.G_MAJOR_PCS)] * 25
        self.assertEqual(self.flags(seq), "CCCC" + "." * 33)

    def test_silent_steps_then_a_borrowed_set_enters(self):
        silent = {"seconds": 1.0, "rms": 0.0, "chroma": [1.0] * 12}
        f_minor_plus_d = [(2, 1.0), (1, 1.0), (5, 0.5), (7, 0.5), (8, 0.5), (10, 0.5), (0, 0.5), (3, 0.5)]
        seq = [self.flat(self.C_MAJOR_PCS)] * 12 + [silent] * 3 + [self.levels(f_minor_plus_d)] * 15
        self.assertEqual(self.flags(seq), "CCCC" + "." * 24 + "CC")

    def test_a_borrowed_note_below_the_lines_stays_out_of_chromatic(self):
        seq = [self.flat(self.C_MAJOR_PCS)] * 10 + [self.flat((0, 2, 4, 5, 7, 9, 11, 1))] * 12
        self.assertEqual(self.flags(seq), "CCCC" + "." * 18)


if __name__ == "__main__":
    unittest.main()
