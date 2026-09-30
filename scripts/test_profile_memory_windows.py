"""Tests for the platform-independent parts of the Windows memory sampler:
phase tracking from the analyzer's stderr and the per-phase summary."""
import unittest

from profile_memory_windows import PHASE_ORDER, phase_after, summarize


class PhaseTests(unittest.TestCase):
    def test_a_phase_line_opens_the_next_phase(self):
        self.assertEqual(phase_after("graph", "include-graph: 12 files, 30 include edges"), "warm")
        self.assertEqual(phase_after("warm", "preprocess: 3 TUs (jobs=8)"), "preprocess")
        self.assertEqual(phase_after("preprocess", "parse: 0 orphan headers, 3 TUs (jobs=8)"), "pch")
        self.assertEqual(phase_after("pch", "pch-done: 0.1s (3 units)"), "tu-merge")
        self.assertEqual(phase_after("tu-merge", "index: 0.2s (3 files, 5 functions, 9 flow)"), "analyze")
        self.assertEqual(phase_after("analyze", "analyze: 0.0s (4 edges, 0 indirect)"), "export")
        self.assertEqual(phase_after("export", "export: 0.0s"), "done")

    def test_other_lines_keep_the_phase(self):
        self.assertEqual(phase_after("pch", "parse: 2/3 src/b.cpp"), "pch")
        self.assertEqual(phase_after("analyze", "warning: solver stopped before convergence"), "analyze")
        self.assertEqual(phase_after("graph", "heap: default"), "graph")

    def test_phases_are_listed_in_pipeline_order(self):
        self.assertEqual(PHASE_ORDER[0], "graph")
        self.assertEqual(PHASE_ORDER[-1], "done")


class SummaryTests(unittest.TestCase):
    # (t, phase, working set, private bytes, cpu)
    SAMPLES = [
        (0.1, "graph", 100, 90, 0.05),
        (0.2, "graph", 300, 280, 0.15),
        (0.3, "preprocess", 250, 240, 0.30),
        (0.4, "preprocess", 500, 450, 0.55),
        (0.5, "analyze", 400, 380, 0.90),
    ]

    def test_peaks_are_per_phase(self):
        per_phase = summarize(self.SAMPLES)
        self.assertEqual(per_phase["graph"]["peak_working_set"], 300)
        self.assertEqual(per_phase["graph"]["peak_private"], 280)
        self.assertEqual(per_phase["preprocess"]["peak_working_set"], 500)
        self.assertEqual(per_phase["analyze"]["peak_working_set"], 400)
        self.assertEqual(per_phase["analyze"]["end_working_set"], 400)
        self.assertEqual(per_phase["analyze"]["end_private"], 380)

    def test_a_straddling_interval_is_charged_to_the_phase_that_ends_in_it(self):
        per_phase = summarize(self.SAMPLES)
        # graph: from t=0 to its last sample at 0.2.
        self.assertAlmostEqual(per_phase["graph"]["t_start"], 0.0)
        self.assertAlmostEqual(per_phase["graph"]["t_end"], 0.2)
        # preprocess starts where graph's last sample was taken, not at its
        # own first sample, so 0.2..0.3 is not dropped.
        self.assertAlmostEqual(per_phase["preprocess"]["t_start"], 0.2)
        self.assertAlmostEqual(per_phase["preprocess"]["t_end"], 0.4)
        self.assertAlmostEqual(per_phase["preprocess"]["cpu_start"], 0.15)
        self.assertAlmostEqual(per_phase["preprocess"]["cpu_end"], 0.55)
        self.assertEqual(per_phase["preprocess"]["samples"], 2)

    def test_no_samples_gives_no_phases(self):
        self.assertEqual(summarize([]), {})


if __name__ == "__main__":
    unittest.main()
