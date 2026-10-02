import sys
import unittest
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import frontend_b as fb  # noqa: E402
from run_frontend_b import first_narrowing  # noqa: E402
import matcher  # noqa: E402


def tone(midis, seconds=4.0, cents=0.0, harmonics=4):
    t = np.arange(int(seconds * fb.RATE)) / fb.RATE
    y = np.zeros_like(t)
    for m in midis:
        f = fb.midi_hz(m + cents / 100.0)
        for h in range(1, harmonics + 1):
            y += np.sin(2 * np.pi * f * h * t) / h
    return (0.1 * y / len(midis)).astype(np.float32)


class FrontendBTests(unittest.TestCase):
    def test_grid_and_frame_timing(self):
        self.assertEqual(fb.N_BINS, 246)
        self.assertAlmostEqual(fb.BIN_PITCH[1], fb.LOW_MIDI)          # centre sub-bin of the first semitone
        times, _, _, _ = fb.analyze(np.zeros(fb.RATE, dtype=np.float32))
        self.assertAlmostEqual(times[0], fb.HOP / fb.RATE)            # first frame ends one hop in
        self.assertAlmostEqual(times[1] - times[0], fb.HOP / fb.RATE)

    def test_single_tone_lands_on_its_pitch_class(self):
        _, c, _, delta = fb.analyze(tone([69]))
        total = c[20:].sum(axis=0)
        self.assertEqual(int(np.argmax(total)), 9)                     # A
        self.assertLess(abs(delta[-1]), 0.05)

    def test_detuned_tone_tuning_estimate_and_class(self):
        _, c, _, delta = fb.analyze(tone([62], cents=30.0))
        self.assertAlmostEqual(float(delta[-1]), 0.30, delta=0.08)
        self.assertEqual(int(np.argmax(c[20:].sum(axis=0))), 2)        # D, despite +30 cents

    def test_major_triad_top_three(self):
        _, c, _, _ = fb.analyze(tone([60, 64, 67]))
        top = set(np.argsort(c[20:].sum(axis=0))[-3:].tolist())
        self.assertEqual(top, {0, 4, 7})

    def test_causal(self):
        a = tone([60], seconds=3.0)
        b = np.concatenate([a[:fb.RATE * 2], tone([66], seconds=1.0)])  # differs only after 2 s
        ta, ca, _, da = fb.analyze(a)
        _, cb, _, db = fb.analyze(b)
        before = ta <= 2.0
        np.testing.assert_allclose(ca[before], cb[before], rtol=1e-5, atol=1e-6)
        np.testing.assert_allclose(da[before], db[before], atol=1e-6)

    def test_silence_is_zero(self):
        _, c, rms, delta = fb.analyze(np.zeros(fb.RATE * 2, dtype=np.float32))
        self.assertEqual(float(np.abs(c).sum()), 0.0)
        self.assertEqual(float(rms.max()), 0.0)
        self.assertEqual(float(np.abs(delta).max()), 0.0)

    def test_b2_tuning_ignores_unresolved_low_bins(self):
        self.assertAlmostEqual(fb.TUNING_MIN_HZ, 138.5, delta=0.5)
        for m in (28, 34, 40):                                         # bass notes with harmonics, in tune
            b1 = abs(fb.analyze(tone([m], seconds=3.0, harmonics=6), "b1")[3][-1])
            b2 = abs(fb.analyze(tone([m], seconds=3.0, harmonics=6), "b2")[3][-1])
            self.assertLess(b2, 0.05)
            self.assertLessEqual(b2, b1 + 0.005)
        self.assertAlmostEqual(float(fb.analyze(tone([40], cents=-30.0, harmonics=6), "b2")[3][-1]), -0.30, delta=0.05)

    def test_cache_revision_guard(self):
        import tempfile
        with tempfile.TemporaryDirectory() as d:
            legacy = Path(d) / "x.npz"
            np.savez(legacy, b_t=np.zeros(1), b_c=np.zeros((1, 12)), b_rms=np.zeros(1), b_delta=np.zeros(1))
            self.assertIn("b_t", fb.load(None, legacy, "b1"))         # no revision field = b1
            with self.assertRaises(ValueError):
                fb.load(None, legacy, "b2")

    def test_pairs_primary_first(self):
        from run_frontend_b import pairs
        self.assertEqual(pairs(["b1", "b2"]), [("B2", "A"), ("B2+V", "A+V"), ("B1", "A"), ("B1+V", "A+V"),
                                               ("B2", "B1"), ("B2+V", "B1+V")])

    def test_first_narrowing(self):
        combos = np.full(10, matcher.CHROMATIC)
        self.assertIsNone(first_narrowing(combos))
        combos[4:] = 3
        self.assertEqual(first_narrowing(combos), 4.0)


if __name__ == "__main__":
    unittest.main()
