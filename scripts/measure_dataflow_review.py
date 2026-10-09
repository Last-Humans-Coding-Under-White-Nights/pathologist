#!/usr/bin/env python3
"""Reproduce PR #196 inspection measurements with a freshly built release binary.

macOS only (uses profile_memory_macos.py); all databases live in TemporaryDirectory.
Example: cargo build --release -p trace-cli
  python3 scripts/measure_dataflow_review.py --corpus /path/to/drivers_hdf_core \
      --baseline /path/to/prior.db --output /tmp/dataflow-review.json
"""
import argparse
from datetime import datetime, timezone
import hashlib
from collections import Counter
from contextlib import closing
import json
from pathlib import Path
import sqlite3
import statistics
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parent.parent


def compare(conn, baseline):
    with closing(sqlite3.connect(baseline)) as old:
        result = {"baseline_run": old.execute("SELECT * FROM analysis_run").fetchone()[:3]}
        for table in ["functions", "variables", "call_sites", "call_edges", "arg_flow_edges",
                      "flow_nodes", "flow_edges", "flow_origins", "flow_calls", "flow_return_calls"]:
            columns = [r[1] for r in conn.execute(f"PRAGMA table_info({table})")]
            prior = {r[1] for r in old.execute(f"PRAGMA table_info({table})")}
            missing = [name for name, fields in [("current", columns), ("baseline", prior)] if not fields]
            if missing:
                result[table] = {"skipped": "table missing in " + " and ".join(missing)}
                continue
            columns = [c for c in columns if c in prior]
            if not columns:
                result[table] = {"skipped": "no shared columns"}
                continue

            def rows(db, fields):
                return Counter(db.execute(f"SELECT {','.join(fields)} FROM {table}"))

            before, after = rows(old, columns), rows(conn, columns)
            result[table] = {"old_rows": sum(before.values()), "new_rows": sum(after.values()),
                             "removed": sum((before-after).values()), "added": sum((after-before).values())}
            ignored = {"variables": "col", "flow_nodes": "fn_id", "flow_origins": "expression"}.get(table)
            if ignored:
                fields = [c for c in columns if c != ignored]
                if fields:
                    result[table]["equal_without_"+ignored] = rows(old, fields) == rows(conn, fields)
                else:
                    result[table]["skipped_without_"+ignored] = "no shared columns after exclusion"
                if ignored == "expression":
                    continue
                missing = [c for c in ["id", "kind"] if c not in columns]
                if missing:
                    result[table]["skipped_changed_kinds"] = "missing shared columns: " + ", ".join(missing)
                    continue
                pk = columns.index("id")
                before_ids = {row[pk]: row for row in before}
                changed = [row for row in after if row[pk] in before_ids and row != before_ids[row[pk]]]
                kind = columns.index("kind")
                result[table]["changed_kinds"] = dict(Counter(row[kind] for row in changed))
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--corpus", type=Path)
    parser.add_argument("--baseline", type=Path)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release/trace")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--nodes", type=int, default=100000)
    parser.add_argument("--runs", type=int, default=3)
    args = parser.parse_args()
    result = {"binary": str(args.binary.resolve()), "binary_sha256": hashlib.sha256(args.binary.read_bytes()).hexdigest(),
              "measured_at": datetime.now(timezone.utc).isoformat(), "synthetic_nodes": args.nodes, "runs": args.runs}
    with tempfile.TemporaryDirectory(prefix="trace-dataflow-review-") as scratch:
        work = Path(scratch)
        def profile(label, command, runs=None):
            samples = []
            for i in range(args.runs if runs is None else runs):
                run = f"{label}-{i}"
                subprocess.run([sys.executable, str(ROOT / "scripts/profile_memory_macos.py"),
                                "--label", run, "--out", str(work), "--interval", "0.001", "--",
                                str(args.binary.resolve()), *map(str, command)], check=True,
                               stdout=subprocess.DEVNULL)
                data = json.loads((work / f"{run}.json").read_text())
                if data["exit"] != 0:
                    raise RuntimeError((work / f"{run}.log").read_text())
                samples.append({key: data[key] for key in ["wall_s", "ru_maxrss_bytes",
                                "lifetime_max_footprint_bytes", "sampled_peak_footprint_bytes"]})
            result[label] = {"samples": samples, "median": {
                key: statistics.median(s[key] for s in samples) for key in samples[0]}}

        def inspect(db, file, line, col, depth=1):
            return ["inspect", db, "dataflow", "--file", file, "--line", line, "--col", col,
                    "--depth", depth, "--format", "json"]

        if args.corpus:
            result["corpus_revision"] = subprocess.check_output(
                ["git", "-C", str(args.corpus), "rev-parse", "HEAD"], text=True).strip()
            result["corpus_status"] = subprocess.check_output(
                ["git", "-C", str(args.corpus), "status", "--porcelain"], text=True)
            db = work / "hdf.db"
            profile("hdf_analyze", ["analyze", args.corpus, "-o", db, "--jobs", "8"], 1)
            with sqlite3.connect(db) as conn:
                result["hdf_provenance"] = {t: conn.execute(f"SELECT count(*) FROM {t}").fetchone()[0]
                                            for t in ["flow_origins", "flow_return_calls", "flow_call_origins"]}
                result["hdf_metadata_checks"] = {
                    "call_targets": conn.execute("SELECT count(*) FROM flow_nodes WHERE kind='call_target'").fetchone()[0],
                    "call_target_caller_mismatches": conn.execute("SELECT count(*) FROM flow_nodes n JOIN call_sites cs ON cs.id=n.call_site_id WHERE n.kind='call_target' AND n.fn_id IS NOT cs.caller_fn_id").fetchone()[0],
                    "sbuf_data_position": conn.execute("SELECT v.line,v.col FROM variables v JOIN files f ON f.id=v.file_id WHERE f.path LIKE '%/hdf_sbuf.c' AND v.name='data' AND v.line=194").fetchall(),
                }
                if args.baseline:
                    result["comparison"] = compare(conn, args.baseline)
            for name, file, line, col in [("can", "can_test.c", 33, 31), ("sbuf", "hdf_sbuf.c", 194, 43)]:
                query = inspect(db, file, line, col)
                profile(name, query)
                graph = json.loads(subprocess.check_output([str(args.binary.resolve()), *map(str, query)], stderr=subprocess.DEVNULL))
                result[name]["graph"] = {"nodes": len(graph["nodes"]), "edges": len(graph["edges"]), "truncated": graph["truncated"],
                                         "roots": [{"name": node["name"], "node_id": node["id"], "location": node["location"]}
                                                   for node in graph["nodes"] if node["depth"] == 0 and node["kind"] in ["parameter", "variable"]]}
                complete_text = subprocess.check_output([str(args.binary.resolve()), *map(str, query[:-1]), "text"], stderr=subprocess.DEVNULL, text=True)
                result[name]["text_lines"] = len(complete_text.splitlines())
                result[name]["text_transitions"] = complete_text.count(" → ")

        db = work / "synthetic.db"
        subprocess.run([str(args.binary.resolve()), "analyze", str(ROOT / "tests/fixtures/dataflow_review"),
                        "-o", str(db), "--jobs", "1"], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        with sqlite3.connect(db) as conn:
            result["fixture_provenance"] = {t: conn.execute(f"SELECT count(*) FROM {t}").fetchone()[0]
                                             for t in ["flow_origins", "flow_return_calls", "flow_call_origins"]}
            for table in ["flow_edges", "flow_call_origins", "flow_origins", "flow_calls", "flow_return_calls", "flow_parameters",
                          "flow_field_access", "flow_field_locations", "flow_nodes", "variables"]:
                conn.execute(f"DELETE FROM {table}")
            conn.execute("UPDATE files SET path='/synthetic.c' WHERE id=0")
            conn.executemany("INSERT INTO variables(id,name,kind,type_id,file_id,line,col,is_synthetic) VALUES(?,?,'global',0,0,?,1,0)",
                             ((i, f"v{i}", i+1) for i in range(args.nodes+2)))
            conn.executemany("INSERT INTO flow_nodes(id,kind,label,var_id) VALUES(?,'var',?,?)",
                             ((i, f"v{i}", i) for i in range(args.nodes+2)))
            conn.executemany("INSERT INTO flow_edges(src_node,dst_node,kind) VALUES(?,?,'copy')",
                             ((i, i+1) for i in range(args.nodes-1)))
            conn.execute("INSERT INTO flow_edges(src_node,dst_node,kind) VALUES(?,?,'copy')", (args.nodes,args.nodes+1))
            conn.commit()
        profile("chain", inspect(db,"synthetic.c",1,1))
        profile("disconnected", inspect(db,"synthetic.c",args.nodes+1,1))
        # Same large component, with technical rather than visible interiors.
        with sqlite3.connect(db) as conn:
            conn.execute("UPDATE variables SET is_synthetic=1 WHERE id>0 AND id<?",(args.nodes-1,))
        profile("hidden_chain", inspect(db,"synthetic.c",1,1),1)
    args.output.write_text(json.dumps(result, indent=2)+"\n")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
