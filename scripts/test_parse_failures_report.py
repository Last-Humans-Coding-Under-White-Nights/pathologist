"""Regression tests for parse-failure captures and deterministic summaries."""

import tempfile
import unittest
from pathlib import Path

from gen_parse_failures_report import categorize_reason, load_tsv, render_corpus, summarize


class ParseInputTests(unittest.TestCase):
    def load(self, text):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "failures.tsv"
            path.write_text(text, encoding="utf-8")
            return load_tsv(path, Path("/project"))

    def test_completed_capture_without_failures_is_valid(self):
        self.assertEqual(self.load("END\t0\n"), {})

    def test_parenthesis_node_kinds_and_snippets_are_preserved(self):
        for node, snippet in [
            ("missing )", ""),
            ("missing (", ""),
            ("missing )", "call(foo) (bar)"),
            ("ERROR", "call(foo) (bar)"),
        ]:
            with self.subTest(node=node, snippet=snippet):
                files = self.load(
                    f"FILE\t/project/a.c\tERROR\tline 1 col 12 ({node}) {snippet}\nEND\t1\n"
                )
                self.assertEqual(files["a.c"]["errors"], [{
                    "line": 1, "col": 12, "node": node, "snippet": snippet,
                }])
                report = "\n".join(render_corpus({"name": "fixture", "root": "/project"}, files))
                self.assertIn(f"`{node}`", report)

    def test_rejects_incomplete_and_foreign_captures(self):
        row = "FILE\t/project/a.c\tERROR\tline 1 col 2 (ERROR) broken\n"
        for text in [
            "", row, row + "END\t2\n", row + "END\t1",
            row + "END\t1\n" + row,
            "foreign\nEND\t1\n",
            "FILE\t\tPARSE\tbroken\nEND\t1\n",
            "FILE\t/project/a.c\tERROR\tbroken\nEND\t1\n",
        ]:
            with self.subTest(text=text), self.assertRaises(SystemExit):
                self.load(text)

    def test_paths_use_components_and_preserve_external_identity(self):
        files = self.load(
            "FILE\t/project/a.c\tPARSE\tfailure\n"
            "FILE\t/project-other/a.c\tPARSE\tfailure\n"
            "FILE\t/dependency/a.c\tPREPROCESS\tmissing include\n"
            "END\t3\n"
        )
        self.assertEqual(set(files), {"a.c", "/project-other/a.c", "/dependency/a.c"})
        self.assertEqual(files["/dependency/a.c"]["note"], "preprocess failed: missing include")

    def test_unicode_line_separators_are_snippet_data(self):
        files = self.load(
            "FILE\t/project/a.c\tERROR\tline 1 col 2 (ERROR) é\u2028😀\nEND\t1\n"
        )
        self.assertEqual(files["a.c"]["errors"][0]["snippet"], "é\u2028😀")

    def test_category_ties_follow_first_node_and_do_not_guess_macro_causes(self):
        errors = [{"node": node, "snippet": ""} for node in ["missing ;", "missing type_identifier"]]
        reason = summarize(errors, None)
        self.assertIn("`missing ;`", reason)
        self.assertEqual(categorize_reason(reason), "missing semicolons")


if __name__ == "__main__":
    unittest.main()
