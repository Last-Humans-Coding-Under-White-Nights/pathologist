"""End-to-end regression for measurement output against the dataflow JSON contract.

Run after cargo build --release -p trace-cli:
  python3 scripts/test_measure_dataflow_review.py
"""
import json
from contextlib import closing
from pathlib import Path
import subprocess
import sqlite3
import sys
import tempfile
import unittest

from measure_dataflow_review import compare

ROOT = Path(__file__).resolve().parent.parent


class BaselineComparisons(unittest.TestCase):
    def compare_schemas(self, before, after):
        with tempfile.TemporaryDirectory() as scratch:
            baseline = Path(scratch) / "baseline.db"
            with closing(sqlite3.connect(baseline)) as old, closing(sqlite3.connect(":memory:")) as new:
                old.executescript("CREATE TABLE analysis_run(id, created_at, version);"
                                  "INSERT INTO analysis_run VALUES(1, 'time', 'version');" + before)
                new.executescript(after)
                old.commit()
                return compare(new, baseline)

    def test_baseline_without_provenance_tables(self):
        result = self.compare_schemas("CREATE TABLE variables(id, kind, col);", """
            CREATE TABLE flow_origins(expression);
            CREATE TABLE flow_calls(call_site_id);
            CREATE TABLE flow_return_calls(call_site_id);
        """)
        for table in ["flow_origins", "flow_calls", "flow_return_calls"]:
            self.assertEqual(result[table], {"skipped": "table missing in baseline"})
        self.assertEqual(result["functions"], {"skipped": "table missing in current and baseline"})
        self.assertEqual(result["variables"], {"skipped": "table missing in current"})

    def test_no_shared_columns(self):
        result = self.compare_schemas("CREATE TABLE functions(old_name);",
                                      "CREATE TABLE functions(new_name);")
        self.assertEqual(result["functions"], {"skipped": "no shared columns"})

    def test_empty_secondary_and_absent_identity_columns(self):
        result = self.compare_schemas("""
            CREATE TABLE variables(col);
            CREATE TABLE flow_nodes(id, fn_id);
            CREATE TABLE flow_origins(expression);
        """, """
            CREATE TABLE variables(col);
            CREATE TABLE flow_nodes(id, fn_id);
            CREATE TABLE flow_origins(expression);
        """)
        for table, column in [("variables", "col"), ("flow_origins", "expression")]:
            self.assertEqual(result[table]["skipped_without_" + column],
                             "no shared columns after exclusion")
            self.assertNotIn("equal_without_" + column, result[table])
        self.assertEqual(result["variables"]["skipped_changed_kinds"], "missing shared columns: id, kind")
        self.assertEqual(result["flow_nodes"]["skipped_changed_kinds"], "missing shared columns: kind")
        self.assertTrue(result["flow_nodes"]["equal_without_fn_id"])

    def test_normal_comparison_counts_duplicate_additions_and_removals(self):
        result = self.compare_schemas("""
            CREATE TABLE variables(id, kind, col);
            INSERT INTO variables VALUES(1, 'local', 1), (2, 'global', 2), (2, 'global', 2);
        """, """
            CREATE TABLE variables(id, kind, col, extra);
            INSERT INTO variables VALUES(1, 'local', 3, 0), (2, 'global', 2, 0),
                                        (3, 'local', 4, 0), (3, 'local', 4, 0);
        """)
        self.assertEqual(result["variables"], {"old_rows": 3, "new_rows": 4, "removed": 2,
                                              "added": 3, "equal_without_col": False,
                                              "changed_kinds": {"local": 1}})


class DataflowMeasurements(unittest.TestCase):
    @unittest.skipUnless(sys.platform == "darwin", "measurement uses the macOS memory observer")
    def test_corpus_queries_collect_roots_from_current_json(self):
        with tempfile.TemporaryDirectory(prefix="trace-measurement-test-") as scratch:
            work = Path(scratch)
            corpus = work / "corpus"
            corpus.mkdir()
            for file, line, name in [("can_test.c", 33, "can_value"),
                                     ("hdf_sbuf.c", 194, "sbuf_value")]:
                (corpus / file).write_text("\n" * (line - 1) + f"char *{name};\n" +
                                          f"void {name}_flow(void) {{ char *copy = {name}; }}\n")
            for command in [
                ["git", "init", "-q", str(corpus)],
                ["git", "-C", str(corpus), "add", "."],
                ["git", "-C", str(corpus), "-c", "user.name=Test",
                 "-c", "user.email=test@example.com", "-c", "commit.gpgsign=false",
                 "commit", "-qm", "measurement fixture"],
            ]:
                subprocess.run(command, check=True, capture_output=True)
            output = work / "measurements.json"
            run = subprocess.run(
                [sys.executable, str(ROOT / "scripts/measure_dataflow_review.py"),
                 "--corpus", str(corpus), "--nodes", "16", "--runs", "1",
                 "--output", str(output)],
                capture_output=True, text=True,
            )
            self.assertEqual(run.returncode, 0, run.stderr)
            report = json.loads(output.read_text())
            for query, name in [("can", "can_value"), ("sbuf", "sbuf_value")]:
                roots = report[query]["graph"]["roots"]
                self.assertEqual(len(roots), 1)
                self.assertEqual(roots[0]["name"], name)
                self.assertIsInstance(roots[0]["node_id"], int)
                self.assertGreater(roots[0]["location"]["line"], 0)
            self.assertIn("hidden_chain", report)


if __name__ == "__main__":
    unittest.main()
