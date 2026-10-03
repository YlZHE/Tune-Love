import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from run_chromatic import GRID2, choose_default, folds, pareto_front  # noqa: E402


def point(mis, cov, **extra):
    return {"first": {"mispull": mis, "coverage": cov}, **extra}


class AnalysisTests(unittest.TestCase):
    def test_grid2_size_and_validity(self):
        self.assertEqual(len(GRID2), 94)
        self.assertTrue(all(g.exit < g.enter for g in GRID2))
        self.assertEqual({g.statistic for g in GRID2}, {"top", "ratio"})

    def test_pareto_drops_dominated_and_keeps_ties(self):
        pts = [point(0.01, 0.30), point(0.02, 0.60), point(0.02, 0.50),   # (0.02, 0.50) is dominated
               point(0.05, 0.90), point(0.05, 0.90), point(0.06, 0.85)]   # duplicates stay; (0.06,0.85) dominated
        keep = pareto_front(pts)
        self.assertEqual(keep, [0, 1, 3, 4])

    def test_default_is_lowest_mispull_with_coverage_at_least_85(self):
        table = [point(0.001, 0.50), point(0.030, 0.86), point(0.020, 0.85), point(0.020, 0.95), point(0.040, 0.99)]
        self.assertEqual(choose_default(table), 3)  # 0.020 twice; higher coverage wins

    def test_default_falls_back_to_highest_coverage(self):
        table = [point(0.001, 0.50), point(0.030, 0.70), point(0.020, 0.70)]
        self.assertEqual(choose_default(table), 2)  # best coverage 0.70 twice; lower mispull wins

    def test_folds_are_contiguous_and_cover_everything(self):
        parts = folds(76, 5)
        self.assertEqual([len(p) for p in parts], [16, 15, 15, 15, 15])
        self.assertEqual([i for p in parts for i in p], list(range(76)))


if __name__ == "__main__":
    unittest.main()
