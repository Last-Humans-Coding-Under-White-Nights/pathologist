"""Regression tests for the conditional coverage TSV reader."""
import tempfile
import unittest
from pathlib import Path

from gen_conditional_coverage_report import load_tsv


class CoverageInputTests(unittest.TestCase):
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
