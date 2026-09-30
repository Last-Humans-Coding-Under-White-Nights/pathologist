"""Tests for the analysis-database digest."""
import sqlite3
import tempfile
import unittest
from pathlib import Path

from db_digest import digest, render


def make_db(path, rows, created_at="2026-10-01T00:00:00Z"):
    """An analysis database with `analysis_run` metadata and a `functions`
    table holding `rows` in insertion order."""
    con = sqlite3.connect(path)
    con.executescript(
        "CREATE TABLE analysis_run (id INTEGER PRIMARY KEY, created_at TEXT NOT NULL);"
        "CREATE TABLE functions (id INTEGER PRIMARY KEY, name TEXT NOT NULL);"
        "CREATE TABLE call_edges (id INTEGER PRIMARY KEY, caller INTEGER);"
    )
    con.execute("INSERT INTO analysis_run VALUES (1, ?)", (created_at,))
    con.executemany("INSERT INTO functions VALUES (?, ?)", rows)
    con.commit()
    con.close()


class DigestTests(unittest.TestCase):
    def test_metadata_does_not_change_the_digest(self):
        with tempfile.TemporaryDirectory() as tmp:
            a, b = Path(tmp) / "a.db", Path(tmp) / "b.db"
            make_db(a, [(1, "main"), (2, "helper")], created_at="2026-10-01T00:00:00Z")
            make_db(b, [(1, "main"), (2, "helper")], created_at="2026-10-02T12:34:56Z")
            self.assertEqual(digest(a), digest(b))

    def test_every_non_metadata_table_is_listed_with_its_row_count(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "a.db"
            make_db(path, [(1, "main"), (2, "helper")])
            tables = digest(path)["tables"]
            self.assertEqual(sorted(tables), ["call_edges", "functions"])
            self.assertEqual(tables["functions"]["rows"], 2)
            self.assertEqual(tables["call_edges"]["rows"], 0)
            self.assertNotIn("analysis_run", tables)

    def test_a_changed_row_changes_the_table_and_overall_digest(self):
        with tempfile.TemporaryDirectory() as tmp:
            a, b = Path(tmp) / "a.db", Path(tmp) / "b.db"
            make_db(a, [(1, "main"), (2, "helper")])
            make_db(b, [(1, "main"), (2, "helpen")])
            da, db = digest(a), digest(b)
            self.assertNotEqual(da["tables"]["functions"]["sha256"], db["tables"]["functions"]["sha256"])
            self.assertEqual(da["tables"]["call_edges"], db["tables"]["call_edges"])
            self.assertNotEqual(da["sha256"], db["sha256"])

    def test_row_order_is_rowid_order_not_insertion_order(self):
        with tempfile.TemporaryDirectory() as tmp:
            a, b = Path(tmp) / "a.db", Path(tmp) / "b.db"
            make_db(a, [(1, "main"), (2, "helper")])
            make_db(b, [(2, "helper"), (1, "main")])
            self.assertEqual(digest(a), digest(b))

    def test_render_is_one_line_per_table_then_the_total(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "a.db"
            make_db(path, [(1, "main")])
            lines = render(digest(path)).splitlines()
            self.assertEqual(len(lines), 3)
            self.assertTrue(lines[0].startswith("call_edges"))
            self.assertIn(" 0 ", lines[0])
            self.assertTrue(lines[1].startswith("functions"))
            self.assertIn(" 1 ", lines[1])
            self.assertTrue(lines[2].startswith("total"))


if __name__ == "__main__":
    unittest.main()
