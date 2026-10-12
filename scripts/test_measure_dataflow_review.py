"""End-to-end regression for measurement output against the dataflow JSON contract.

Run after cargo build --release -p trace-cli:
  python3 scripts/test_measure_dataflow_review.py
"""
import json
import io
from contextlib import redirect_stdout
from contextlib import closing
from pathlib import Path
import subprocess
import sqlite3
import sys
import tempfile
import unittest
from unittest.mock import patch

import measure_dataflow_review
from measure_dataflow_review import compare, synthetic_graph
from analysis_sqlite import storage_relation

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

    def test_compare_reads_logical_views_in_v7_v8_and_v8_v8_pairs(self):
        legacy = """
            CREATE TABLE variables(id,name,kind,col);
            INSERT INTO variables VALUES(1,'value','param',2);
            CREATE TABLE flow_nodes(id,kind,fn_id);
            INSERT INTO flow_nodes VALUES(1,'var',7);
            CREATE TABLE flow_edges(src_node,dst_node,kind);
            INSERT INTO flow_edges VALUES(1,2,'copy');
            CREATE TABLE flow_origins(src_node,expression);
            INSERT INTO flow_origins VALUES(1,'result = value'),(1,'result = value');
        """

        def compact(offset):
            return f"""
                CREATE TABLE strings(id INTEGER PRIMARY KEY,text);
                INSERT INTO strings VALUES({offset+1},'value'),({offset+2},'var'),
                    ({offset+3},'copy'),({offset+4},'result = value');
                CREATE TABLE variables_data(id,name_id,kind,col);
                INSERT INTO variables_data VALUES(1,{offset+1},'param',2);
                CREATE VIEW variables AS SELECT id,
                    (SELECT text FROM strings WHERE id=name_id) AS name,kind,col FROM variables_data;
                CREATE TABLE flow_nodes_data(id,kind_id,fn_id);
                INSERT INTO flow_nodes_data VALUES(1,{offset+2},7);
                CREATE VIEW flow_nodes AS SELECT id,
                    (SELECT text FROM strings WHERE id=kind_id) AS kind,fn_id FROM flow_nodes_data;
                CREATE TABLE flow_edges_data(src_node,dst_node,kind_id);
                INSERT INTO flow_edges_data VALUES(1,2,{offset+3});
                CREATE VIEW flow_edges AS SELECT src_node,dst_node,
                    (SELECT text FROM strings WHERE id=kind_id) AS kind FROM flow_edges_data;
                CREATE TABLE flow_origins_data(src_node,expression_id);
                INSERT INTO flow_origins_data VALUES(1,{offset+4}),(1,{offset+4});
                CREATE VIEW flow_origins AS SELECT src_node,
                    (SELECT text FROM strings WHERE id=expression_id) AS expression FROM flow_origins_data;
            """

        for before, after in [(legacy, compact(10)), (compact(10), compact(100))]:
            result = self.compare_schemas(before, after)
            for relation in ["variables", "flow_nodes", "flow_edges", "flow_origins"]:
                self.assertNotIn("skipped", result[relation])
                self.assertEqual(result[relation]["removed"], 0)
                self.assertEqual(result[relation]["added"], 0)
            self.assertEqual(result["flow_origins"]["new_rows"], 2)
            self.assertTrue(result["variables"]["equal_without_col"])
            self.assertTrue(result["flow_nodes"]["equal_without_fn_id"])
            self.assertTrue(result["flow_origins"]["equal_without_expression"])
            self.assertNotIn("strings", result)
            self.assertNotIn("variables_data", result)


class SyntheticGraphs(unittest.TestCase):
    def test_legacy_graph_and_hidden_chain_remain_writable(self):
        with closing(sqlite3.connect(":memory:")) as conn:
            conn.executescript("""
                CREATE TABLE files(id INTEGER PRIMARY KEY,path);
                INSERT INTO files VALUES(0,'fixture.c');
                CREATE TABLE variables(id INTEGER PRIMARY KEY,name,kind,type_id,file_id,line,col,is_synthetic);
                CREATE TABLE flow_nodes(id INTEGER PRIMARY KEY,kind,label,var_id);
                CREATE TABLE flow_edges(id INTEGER PRIMARY KEY,src_node,dst_node,kind);
            """)
            for table in ["flow_call_origins", "flow_origins", "flow_calls", "flow_return_calls",
                          "flow_parameters", "flow_field_access", "flow_field_locations"]:
                conn.execute(f"CREATE TABLE {table}(id)")
            synthetic_graph(conn, 16)
            self.assertEqual(conn.execute("SELECT count(*) FROM variables").fetchone()[0], 18)
            self.assertEqual(conn.execute("SELECT count(*) FROM flow_nodes").fetchone()[0], 18)
            self.assertEqual(conn.execute("SELECT count(*) FROM flow_edges").fetchone()[0], 16)
            self.assertEqual(conn.execute("SELECT src_node,dst_node,kind FROM flow_edges ORDER BY id").fetchall(),
                             [(i, i + 1, "copy") for i in range(15)] + [(16, 17, "copy")])
            table = storage_relation(conn, "variables")
            conn.execute(f"UPDATE {table} SET is_synthetic=1 WHERE id>0 AND id<15")
            self.assertEqual(conn.execute("SELECT count(*) FROM variables WHERE is_synthetic=1").fetchone()[0], 14)


class DataflowMeasurements(unittest.TestCase):
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
            argv = [str(ROOT / "scripts/measure_dataflow_review.py"),
                    "--corpus", str(corpus), "--nodes", "2000", "--runs", "2",
                    "--output", str(output)]
            if sys.platform == "darwin":
                run = subprocess.run([sys.executable, *argv], capture_output=True, text=True)
                self.assertEqual(run.returncode, 0, run.stderr)
            else:
                # Exercise all database rewrites, real CLI commands and JSON
                # parsing without requiring the macOS-only memory observer.
                real_run = subprocess.run

                def observe(command, **kwargs):
                    if Path(command[1]).name != "profile_memory_macos.py":
                        return real_run(command, **kwargs)
                    run = real_run(command[command.index("--") + 1:], capture_output=True, text=True)
                    label = command[command.index("--label") + 1]
                    directory = Path(command[command.index("--out") + 1])
                    (directory / f"{label}.json").write_text(json.dumps({
                        "exit": run.returncode, "wall_s": 0, "ru_maxrss_bytes": 0,
                        "lifetime_max_footprint_bytes": 0, "sampled_peak_footprint_bytes": 0}))
                    (directory / f"{label}.log").write_text(run.stdout + run.stderr)
                    return run

                with patch.object(sys, "argv", argv), \
                        patch.object(measure_dataflow_review.subprocess, "run", side_effect=observe), \
                        redirect_stdout(io.StringIO()):
                    measure_dataflow_review.main()
            report = json.loads(output.read_text())
            self.assertEqual(report["synthetic_nodes"], 2000)
            self.assertEqual(len(report["chain"]["samples"]), 2)
            self.assertEqual(len(report["disconnected"]["samples"]), 2)
            for query, name in [("can", "can_value"), ("sbuf", "sbuf_value")]:
                roots = report[query]["graph"]["roots"]
                self.assertEqual(len(roots), 1)
                self.assertEqual(roots[0]["name"], name)
                self.assertIsInstance(roots[0]["node_id"], int)
                self.assertGreater(roots[0]["location"]["line"], 0)
            self.assertIn("hidden_chain", report)


if __name__ == "__main__":
    unittest.main()
