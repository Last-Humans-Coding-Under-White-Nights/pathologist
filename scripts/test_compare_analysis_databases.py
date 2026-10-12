"""Logical comparison regressions, including compact views and composite keys.

Run: python3 scripts/test_compare_analysis_databases.py
"""
from contextlib import closing
import json
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import unittest

from analysis_sqlite import logical_relations, storage_relation
from compare_analysis_databases import compare_databases
from measure_overload_provenance import analysis_rows

ROOT = Path(__file__).resolve().parent.parent


def fixture(path, compact, offset=0):
    with closing(sqlite3.connect(path)) as conn:
        conn.executescript("""
            CREATE TABLE analysis_run(id INTEGER PRIMARY KEY,trace_version,schema_version,
                                      created_at,target_root,options_json);
            CREATE TABLE flow_field_access(base_node INTEGER,dst_node INTEGER,field_name TEXT,
                                           PRIMARY KEY(base_node,dst_node,field_name))
        """ + ("WITHOUT ROWID;" if compact else ";"))
        conn.execute("INSERT INTO analysis_run VALUES(1,?,?,?,'/src','{}')",
                     ("compact" if compact else "legacy", 8 if compact else 7,
                      "after" if compact else "before"))
        conn.executemany("INSERT INTO flow_field_access VALUES(?,?,?)",
                         [(2, 4, "second"), (1, 3, "first")])
        if compact:
            conn.executescript("""
                CREATE TABLE strings(id INTEGER PRIMARY KEY,text TEXT);
                CREATE TABLE flow_nodes_data(id INTEGER PRIMARY KEY,kind_id,label_id);
                CREATE VIEW flow_nodes AS SELECT id,
                    (SELECT text FROM strings WHERE id=kind_id) AS kind,
                    (SELECT text FROM strings WHERE id=label_id) AS label FROM flow_nodes_data;
                CREATE TABLE flow_edges_data(id INTEGER PRIMARY KEY,src_node,dst_node,kind_id);
                CREATE VIEW flow_edges AS SELECT id,src_node,dst_node,
                    (SELECT text FROM strings WHERE id=kind_id) AS kind FROM flow_edges_data;
                CREATE TABLE flow_origins_data(src_node,dst_node,expression_id);
                CREATE VIEW flow_origins AS SELECT src_node,dst_node,
                    (SELECT text FROM strings WHERE id=expression_id) AS expression FROM flow_origins_data;
            """)
            conn.executemany("INSERT INTO strings VALUES(?,?)",
                             [(offset + i, text) for i, text in enumerate(
                                 ["var", "a", "b", "copy", 'b = a /* Привет " */'], 1)])
            conn.executemany("INSERT INTO flow_nodes_data VALUES(?,?,?)",
                             [(2, offset + 1, offset + 3), (1, offset + 1, offset + 2)])
            conn.execute("INSERT INTO flow_edges_data VALUES(1,1,2,?)", [offset + 4])
            conn.executemany("INSERT INTO flow_origins_data VALUES(1,2,?)", [(offset + 5,)] * 2)
        else:
            conn.executescript("""
                CREATE TABLE flow_nodes(id INTEGER PRIMARY KEY,kind,label);
                INSERT INTO flow_nodes VALUES(2,'var','b'),(1,'var','a');
                CREATE TABLE flow_edges(id INTEGER PRIMARY KEY,src_node,dst_node,kind);
                INSERT INTO flow_edges VALUES(1,1,2,'copy');
                CREATE TABLE flow_origins(src_node,dst_node,expression);
            """)
            conn.executemany("INSERT INTO flow_origins VALUES(1,2,?)", [('b = a /* Привет " */',)] * 2)
        conn.execute("CREATE VIEW flow_nodes_text AS SELECT * FROM flow_nodes")
        conn.commit()


class LogicalComparisons(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.addCleanup(self.scratch.cleanup)
        self.before = Path(self.scratch.name) / "before.db"
        self.after = Path(self.scratch.name) / "after.db"
        fixture(self.before, False)
        fixture(self.after, True)

    def test_discovery_includes_public_views_and_excludes_storage(self):
        with closing(sqlite3.connect(self.after)) as conn:
            self.assertEqual(logical_relations(conn),
                             ["analysis_run", "flow_edges", "flow_field_access", "flow_nodes", "flow_origins"])
            self.assertEqual(storage_relation(conn, "flow_edges"), "flow_edges_data")
            self.assertEqual(storage_relation(conn, "flow_field_access"), "flow_field_access")

    def test_v7_and_v8_match_without_rowids_or_run_metadata(self):
        self.assertEqual(analysis_rows(self.before), analysis_rows(self.after))
        report = compare_databases(self.before, self.after)
        self.assertTrue(report["equal"])
        self.assertEqual(report["relations"]["flow_origins"]["rows_after"], 2)

    def test_v8_matches_v8_with_different_dictionary_ids(self):
        other = Path(self.scratch.name) / "other.db"
        fixture(other, True, 100)
        self.assertEqual(analysis_rows(self.after), analysis_rows(other))
        self.assertTrue(compare_databases(self.after, other)["equal"])

    def test_different_insertion_order_is_equal(self):
        with closing(sqlite3.connect(self.before)) as conn:
            conn.execute("DELETE FROM flow_field_access")
            conn.executemany("INSERT INTO flow_field_access VALUES(?,?,?)",
                             [(1, 3, "first"), (2, 4, "second")])
            conn.commit()
        self.assertEqual(analysis_rows(self.before), analysis_rows(self.after))

    def test_duplicate_removal_is_detected(self):
        with closing(sqlite3.connect(self.after)) as conn:
            conn.execute("DELETE FROM flow_origins_data WHERE rowid=1")
            conn.commit()
        report = compare_databases(self.before, self.after)
        self.assertFalse(report["equal"])
        self.assertEqual(report["relations"]["flow_origins"]["rows_before"], 2)
        self.assertEqual(report["relations"]["flow_origins"]["rows_after"], 1)

    def test_expression_and_entity_ids_are_compared(self):
        with closing(sqlite3.connect(self.after)) as conn:
            conn.execute("UPDATE strings SET text='changed expression' WHERE id=5")
            conn.execute("UPDATE flow_nodes_data SET id=99 WHERE id=2")
            conn.commit()
        report = compare_databases(self.before, self.after)
        self.assertFalse(report["relations"]["flow_origins"]["equal"])
        self.assertFalse(report["relations"]["flow_nodes"]["equal"])

    def test_options_missing_relations_and_changed_columns_are_detected(self):
        with closing(sqlite3.connect(self.after)) as conn:
            conn.execute("UPDATE analysis_run SET options_json='different'")
            conn.execute("DROP VIEW flow_edges")
            conn.execute("ALTER TABLE flow_field_access ADD COLUMN extra")
            conn.commit()
        report = compare_databases(self.before, self.after)
        self.assertFalse(report["relations"]["analysis_run"]["equal"])
        self.assertEqual(report["relations"]["flow_edges"]["missing"], "after")
        self.assertFalse(report["relations"]["flow_field_access"]["equal"])

    def test_cli_reports_success_and_failure_without_creating_inputs(self):
        command = [sys.executable, str(ROOT / "scripts/compare_analysis_databases.py"),
                   str(self.before), str(self.after)]
        output = Path(self.scratch.name) / "comparison.json"
        result = subprocess.run([*command, "--output", str(output)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(json.loads(output.read_text())["equal"])
        with closing(sqlite3.connect(self.after)) as conn:
            conn.execute("DELETE FROM flow_origins_data")
            conn.commit()
        result = subprocess.run(command, capture_output=True, text=True)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertFalse(json.loads(result.stdout)["equal"])
        absent = Path(self.scratch.name) / "absent.db"
        result = subprocess.run([*command[:2], str(absent), str(self.after)], capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(absent.exists())


if __name__ == "__main__":
    unittest.main()
