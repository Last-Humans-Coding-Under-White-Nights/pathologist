"""Portable v7/v8 field sampling and dictionary-filter regressions."""
from contextlib import closing
from pathlib import Path
import sqlite3
import tempfile
import unittest

from slice_sizes import sample_query, starts


def fixture(path, compact):
    with closing(sqlite3.connect(path)) as conn:
        conn.executescript("""
            CREATE TABLE files(id INTEGER PRIMARY KEY,path TEXT);
            INSERT INTO files VALUES(1,'/src/prod/main.cpp'),(2,'/src/test/main.cpp');
            CREATE TABLE flow_memory_access(edge_id INTEGER,cell_node INTEGER);
            INSERT INTO flow_memory_access VALUES(1,100),(2,101),(3,102),(4,103),(5,104);
        """)
        labels = ["summary:A.first", "summary:A.second", "summary:A.read",
                  "concrete field", "summary:A.test"]
        if compact:
            conn.executescript("""
                CREATE TABLE strings(id INTEGER PRIMARY KEY,text TEXT);
                CREATE TABLE flow_edges_data(id INTEGER PRIMARY KEY,src_node,dst_node,kind_id);
                CREATE VIEW flow_edges AS SELECT id,src_node,dst_node,
                    (SELECT text FROM strings WHERE id=kind_id) AS kind FROM flow_edges_data;
                CREATE TABLE flow_nodes_data(id INTEGER PRIMARY KEY,label_id,detail_id);
                CREATE VIEW flow_nodes AS SELECT id,
                    (SELECT text FROM strings WHERE id=label_id) AS label,
                    (SELECT text FROM strings WHERE id=detail_id) AS detail FROM flow_nodes_data;
                CREATE TABLE flow_origins_data(src_node,dst_node,kind_id,file_id,line,col);
                CREATE VIEW flow_origins AS SELECT src_node,dst_node,
                    (SELECT text FROM strings WHERE id=kind_id) AS kind,file_id,line,col FROM flow_origins_data;
            """)
            # IDs deliberately differ from TextPool's reserved prefix.
            conn.executemany("INSERT INTO strings VALUES(?,?)", enumerate(
                ["store", "field_summary", "field", "load", *labels], 501))
            conn.executemany("INSERT INTO flow_nodes_data VALUES(?,?,?)",
                             [(100+i, 505+i, 503 if i == 3 else 502) for i in range(5)])
            edges, origins = "flow_edges_data", "flow_origins_data"
            store, load = 501, 504
        else:
            conn.executescript("""
                CREATE TABLE flow_edges(id INTEGER PRIMARY KEY,src_node,dst_node,kind);
                CREATE TABLE flow_nodes(id INTEGER PRIMARY KEY,label,detail);
                CREATE TABLE flow_origins(src_node,dst_node,kind,file_id,line,col);
            """)
            conn.executemany("INSERT INTO flow_nodes VALUES(?,?,?)",
                             [(100+i, label, "field" if i == 3 else "field_summary")
                              for i, label in enumerate(labels)])
            edges, origins = "flow_edges", "flow_origins"
            store, load = "store", "load"
        for i in range(1, 6):
            kind = load if i == 3 else store
            conn.execute(f"INSERT INTO {edges} VALUES(?,?,?,?)", (i, 10+i, 20+i, kind))
            conn.execute(f"INSERT INTO {origins} VALUES(?,?,?,?,?,?)",
                         (10+i, 20+i, kind, 2 if i == 5 else 1, i+7, 1))
        # Sampling retains the first stored site rather than another statement.
        conn.execute(f"INSERT INTO {origins} VALUES(11,21,?,1,50,1)", [store])
        conn.execute(f"CREATE INDEX origin_src ON {origins}(src_node)")
        conn.commit()


class FieldSampling(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.addCleanup(self.scratch.cleanup)
        self.legacy = Path(self.scratch.name) / "v7.db"
        self.compact = Path(self.scratch.name) / "v8.db"
        fixture(self.legacy, False)
        fixture(self.compact, True)

    def test_v7_and_v8_choose_identical_sites_and_exclusions(self):
        for sample in [1, 2, 16]:
            self.assertEqual(starts(self.legacy, sample), starts(self.compact, sample))
        total, rows = starts(self.compact, 16)
        self.assertEqual(total, 2)
        self.assertEqual(rows, [("summary:A.first", "/src/prod/main.cpp", 8, 1),
                                ("summary:A.second", "/src/prod/main.cpp", 9, 2)])

    def test_tags_are_bound_once_and_not_decoded_in_filters(self):
        with closing(sqlite3.connect(self.compact)) as conn:
            statements = []
            conn.set_trace_callback(statements.append)
            sql, params = sample_query(conn)
            self.assertEqual(params, (501, 502))
            self.assertIn("e.kind_id = ? AND n.detail_id = ?", sql)
            self.assertNotIn("n.detail =", sql)
            self.assertNotIn("WHERE text=", sql)
            conn.execute(sql, params).fetchall()
            lookups = [s for s in statements if "FROM strings WHERE text=" in s]
            self.assertEqual(len(lookups), 2)
            plan = [row[3] for row in conn.execute("EXPLAIN QUERY PLAN " + sql, params)]
            self.assertFalse(any("SCAN strings" in step for step in plan), plan)

    def test_empty_graph_without_unused_tags_has_no_sample(self):
        with closing(sqlite3.connect(self.compact)) as conn:
            for table in ["flow_memory_access", "flow_edges_data", "flow_origins_data", "flow_nodes_data"]:
                conn.execute(f"DELETE FROM {table}")
            conn.execute("DELETE FROM strings WHERE text='field_summary'")
            conn.commit()
        self.assertEqual(starts(self.compact, 16), (0, []))


if __name__ == "__main__":
    unittest.main()
