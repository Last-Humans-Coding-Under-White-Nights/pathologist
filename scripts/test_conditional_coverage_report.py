"""Regression tests for the conditional coverage TSV reader."""
import tempfile
import unittest
from pathlib import Path

from gen_conditional_coverage_report import load_tsv, render_corpus


class CoverageInputTests(unittest.TestCase):
    def test_gn_candidates_preserve_evidence_and_rank_by_confidence(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "coverage.tsv"
            path.write_text(
                "META\tdefines\t\n"
                "GN_DEFINE\tDYNAMIC\t1\t$value\tflags.gni\t9\tlow\t(enabled)\n"
                "GN_DEFINE\tLITERAL\t0\t\tBUILD.gn\t3\thigh\t\n"
                "END\t3\n"
            )
            corpus = load_tsv(path)
            report = "\n".join(render_corpus({"name": "fixture", "root": tmp}, corpus))
            self.assertLess(report.index("`LITERAL`"), report.index("`DYNAMIC`"))
            self.assertIn("`flags.gni:9`", report)
            self.assertIn("`$value`", report)
            self.assertIn("(enabled)", report)

    def test_gn_candidate_lines_separate_unread_names_from_zero_line_names(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "coverage.tsv"
            path.write_text(
                "META\tdefines\t\n"
                "FILE\tx.c\ttu\t9\t1\n"
                "CHAIN\tx.c\t1\t0\t0\t1\t1\n"
                # Taken by every run, so the chain reads the name but excludes
                # no lines: distinct from a name no chain reads at all.
                "ARM\tx.c\t1\t0\tif\t1\t3\t1\t0\t0\t1\tREAD_ONLY\n"
                "READ\tx.c\t1\t0\tREAD_ONLY\t1\t0\t0\n"
                "GN_DEFINE\tREAD_ONLY\t0\t\tBUILD.gn\t5\thigh\t\n"
                "GN_DEFINE\tUNREAD\t0\t\tBUILD.gn\t7\thigh\t\n"
                "END\t7\n"
            )
            corpus = load_tsv(path)
            report = "\n".join(render_corpus({"name": "fixture", "root": tmp}, corpus))
            self.assertIn("| `BUILD.gn:5` | high | \u2014 | 0 / 0 |", report)
            self.assertIn("| `BUILD.gn:7` | high | \u2014 | \u2014 |", report)

    def test_rejects_capture_cut_at_a_complete_row(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "coverage.tsv"
            for text in ["META\tdefines\t\n", "META\tdefines\t\nFILE\tx.c\ttu\t3\t1\n"]:
                with self.subTest(text=text):
                    path.write_text(text)
                    with self.assertRaisesRegex(SystemExit, "completion"):
                        load_tsv(path)

    def test_rejects_incorrect_row_count_and_trailing_records(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "coverage.tsv"
            for suffix in ["END\t1\n", "END\t2\nFILE\ty.c\ttu\t3\t1\n", "END\tnan\n"]:
                with self.subTest(suffix=suffix):
                    path.write_text("META\tdefines\t\nFILE\tx.c\ttu\t3\t1\n" + suffix)
                    with self.assertRaisesRegex(SystemExit, "completion"):
                        load_tsv(path)

    def test_complete_file_without_conditionals_is_valid(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "coverage.tsv"
            path.write_text("META\tdefines\t\nFILE\tx.c\ttu\t3\t1\nEND\t2\n")
            self.assertEqual(load_tsv(path).files, {"x.c": ("tu", 3, 1)})

    def test_carriage_return_is_field_data(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "coverage.tsv"
            path.write_bytes(
                b"META\tdefines\t\nFILE\tx.c\ttu\t3\t1\n"
                b"CHAIN\tx.c\t1\t0\t0\t1\t1\n"
                b"ARM\tx.c\t1\t0\tif\t1\t3\t0\t1\t0\t1\tA\r && B\n"
                b"END\t4\n"
            )
            corpus = load_tsv(path)
            self.assertEqual(corpus.chains[("x.c", 1)].arms[0].expression, "A\r && B")


if __name__ == "__main__":
    unittest.main()
