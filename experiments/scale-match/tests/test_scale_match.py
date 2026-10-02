import sys
import unittest
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import lrc  # noqa: E402
import matcher  # noqa: E402
from metrics import combined, consonance, note_frames, snap, snap_pcs, success_table, summarize, tuning_offset  # noqa: E402
from scales import SCALES, all_combos, label, mask, transpose  # noqa: E402


def evidence_for(pcs, seconds=60.0, leak=0.0):
    ev = np.full(12, leak * seconds / 12)
    for pc in pcs:
        ev[pc] += seconds / len(pcs)
    return ev


class ScaleTests(unittest.TestCase):
    def test_profile_order_and_sizes(self):
        self.assertEqual(len(SCALES), 15)
        self.assertEqual(list(SCALES)[0], "Chromatic")
        self.assertEqual(list(SCALES)[-1], "Diminished")
        self.assertEqual(mask(9, "Minor").tolist(), mask(0, "Major").tolist())  # A minor == C major notes

    def test_combo_count(self):
        combos, masks = all_combos()
        self.assertEqual(len(combos), 14 * 12 + 1)
        self.assertEqual(masks.shape, (169, 12))

    def test_transpose_shifts_key_only(self):
        self.assertEqual(transpose(0, "Major", 2), (2, "Major"))
        self.assertEqual(transpose(11, "Minor", 2), (1, "Minor"))


class MatcherTests(unittest.TestCase):
    def choose(self, ev, **kw):
        params = matcher.Params(**kw)
        idx, _ = matcher.best(matcher.normalized(ev, params.alpha), params)
        return label(*matcher.COMBOS[idx])

    def test_c_major_evidence_picks_c_major(self):
        self.assertEqual(self.choose(evidence_for([0, 2, 4, 5, 7, 9, 11])), "C Major")

    def test_pentatonic_evidence_picks_pentatonic(self):
        self.assertEqual(self.choose(evidence_for([7, 9, 11, 2, 4])), "G Major Pentatonic")

    def test_diatonic_family_excludes_pentatonic(self):
        # Pentatonic notes fit C, G and D Major equally; any 7-note set covering them is valid.
        params = matcher.Params(family="diatonic")
        idx, _ = matcher.best(matcher.normalized(evidence_for([7, 9, 11, 2, 4]), params.alpha), params)
        chosen = matcher.MASKS[idx]
        self.assertEqual(int(chosen.sum()), 7)
        self.assertTrue(chosen[[7, 9, 11, 2, 4]].all())

    def test_weak_evidence_stays_chromatic(self):
        self.assertEqual(self.choose(evidence_for([0, 2, 4, 5, 7, 9, 11], seconds=0.5)), "Chromatic")

    def test_ambiguous_evidence_stays_chromatic(self):
        self.assertEqual(self.choose(evidence_for(range(12))), "Chromatic")

    def test_hysteresis_keeps_current_on_small_gain(self):
        tracker = matcher.Tracker(matcher.Params(margin=10.0))
        self.assertEqual(tracker.update(evidence_for([0, 2, 4, 5, 7, 9, 11])), matcher.CHROMATIC)


class MetricTests(unittest.TestCase):
    def test_snap(self):
        c_major = mask(0, "Major")
        self.assertEqual(snap(60.6, c_major), 60)  # C# not allowed -> back to C
        self.assertEqual(snap(64.6, c_major), 65)  # E -> F is a semitone: 60 cents snaps to F
        self.assertEqual(snap(60.6, np.ones(12, bool)), 61)

    def test_summary_mispull_and_correction(self):
        table = success_table(matcher.MASKS)
        c_major = matcher.COMBOS.index((0, "Major"))
        pcs = np.array([0, 1])  # C is in the set, C# is not
        result = summarize(pcs, np.array([c_major, c_major]), table)
        self.assertAlmostEqual(result["mispull"], 0.5)
        chrom = summarize(pcs, np.array([matcher.CHROMATIC] * 2), table)
        self.assertEqual(chrom["mispull"], 0.0)
        self.assertEqual(chrom["byDeviation"]["60"], 0.0)
        self.assertEqual(chrom["byDeviation"]["30"], 1.0)

    def test_snap_pcs_matches_scalar_snap(self):
        sung = np.array([60.6, 64.6, 61.4, 70.2])
        masks = np.array([mask(0, "Major")] * 4)
        self.assertEqual(snap_pcs(sung, masks).tolist(), [snap(x, masks[0]) % 12 for x in sung])

    def test_consonance_rewards_narrowed_set(self):
        # Speech-like pitch drifting on C#; accompaniment sounds C-E-G.
        sung = np.full(10, 61.2)
        harmony = np.zeros((10, 12), bool)
        harmony[:, [0, 4, 7]] = True
        c_major = matcher.COMBOS.index((0, "Major"))
        chrom = consonance(sung, np.full(10, matcher.CHROMATIC), matcher.MASKS, harmony)
        major = consonance(sung, np.full(10, c_major), matcher.MASKS, harmony)
        self.assertGreater(major["consonant"], chrom["consonant"])

    def test_combined_weights_by_frames(self):
        self.assertEqual(combined({"frames": 3, "outputCorrect": 1.0}, {"frames": 1, "consonant": 0.0}), 0.75)
        self.assertEqual(combined({"frames": 0, "outputCorrect": None}, {"frames": 2, "consonant": 0.5}), 0.5)

    def test_tuning_offset(self):
        self.assertAlmostEqual(tuning_offset(np.array([60.2, 62.2, 64.2])), 0.2, places=5)

    def test_note_frames_rejects_glides(self):
        hop = 0.01
        steady = np.full(30, 60.0)
        glide = np.linspace(62.0, 66.0, 30)
        midi = np.concatenate([steady, glide])
        voiced = np.ones(60, bool)
        ev, note, _ = note_frames(np.arange(60) * hop, midi, voiced, hop)
        self.assertTrue(ev[:25].all())
        self.assertFalse(ev[35:].any())


class LrcTests(unittest.TestCase):
    def test_parse_and_blank_lines(self):
        lines = lrc.parse("[00:31.922]a\n[00:32.926]\n[00:33.506]b\n[ti:x]\n")
        self.assertEqual(lines[0], (31.922, 32.926, "a"))
        self.assertEqual(lines[1], (33.506, None, "b"))
        self.assertEqual(lrc.first_line(lines), (31.922, 32.926))

    def test_two_digit_fraction(self):
        self.assertEqual(lrc.parse("[01:02.50]x")[0][0], 62.5)


if __name__ == "__main__":
    unittest.main()


class NoChromaticTests(unittest.TestCase):
    def test_bypass_metric_only_credits_in_tune_notes(self):
        table = success_table(matcher.MASKS, chromatic_corrects=False)
        chrom = summarize(np.array([0, 1]), np.array([matcher.CHROMATIC] * 2), table)
        self.assertEqual(chrom["byDeviation"]["30"], 0.0)
        self.assertEqual(chrom["mispull"], 0.0)
        c_major = matcher.COMBOS.index((0, "Major"))
        major = summarize(np.array([0]), np.array([c_major]), table)
        self.assertEqual(major["byDeviation"]["30"], 1.0)

    def test_majmin_commits_on_first_evidence_and_never_picks_chromatic(self):
        tracker = matcher.Tracker(matcher.Params(family="majmin"))
        self.assertEqual(tracker.update(np.zeros(12)), matcher.CHROMATIC)  # undecided: nothing written
        idx = tracker.update(evidence_for([0, 2, 4, 5, 7, 9, 11], seconds=1.0))
        self.assertNotEqual(idx, matcher.CHROMATIC)
        self.assertIn(matcher.COMBOS[idx][1], ("Major", "Minor"))
        for _ in range(5):
            idx = tracker.update(evidence_for(range(12), seconds=60))
            self.assertNotEqual(idx, matcher.CHROMATIC)
