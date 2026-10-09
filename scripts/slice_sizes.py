#!/usr/bin/env python3
"""Slice sizes on an analysis database (#205, docs/EVAL_REPORT.md).

Takes a deterministic sample of struct and class fields that some store
writes (a store's `flow_memory_access` cell that is a field summary, outside
test directories, at the store's `flow_origins` position), starts `trace inspect slice` at the
first such store of each, and prints the size of every slice and a summary:
nodes, edges, cross-context edges, truncated stages, contexts and wall time.

Usage:
    python3 scripts/slice_sizes.py DB [--bin target/release/trace] [--sample 40]
        [--up-depth 6] [--down-depth 6]

Uses only the stdlib.
"""

import argparse
import json
import sqlite3
import statistics
import subprocess
import sys
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

TEST_DIRS = ("/test/", "/tests/", "/unittest/", "/fuzztest/", "/mock/", "/demo/")


def starts(db, sample):
    con = sqlite3.connect(db)
    rows = con.execute(
        "SELECT n.label, p.path, o.line, MIN(o.rowid) FROM flow_memory_access a "
        "JOIN flow_edges e ON e.id = a.edge_id JOIN flow_nodes n ON n.id = a.cell_node "
        "JOIN flow_origins o ON o.src_node = e.src_node AND o.dst_node = e.dst_node "
        "AND o.kind = e.kind JOIN files p ON p.id = o.file_id "
        "WHERE e.kind = 'store' AND n.detail = 'field_summary' "
        "GROUP BY n.id ORDER BY n.label, p.path, o.line").fetchall()
    rows = [r for r in rows if not any(d in r[1] for d in TEST_DIRS)]
    step = max(1, len(rows) // sample)
    return len(rows), rows[::step][:sample]


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("db")
    ap.add_argument("--bin", default=str(REPO_ROOT / "target" / "release" / "trace"))
    ap.add_argument("--sample", type=int, default=40)
    ap.add_argument("--up-depth", type=int, default=6)
    ap.add_argument("--down-depth", type=int, default=6)
    args = ap.parse_args()
    total, picked = starts(args.db, args.sample)
    results = []
    for label, path, line, _ in picked:
        name = label.rsplit(".", 1)[-1]
        try:
            row = Path(path).read_text(errors="replace").splitlines()[line - 1]
            col = row.find(name) + 1 or 1
        except (OSError, IndexError):
            col = 1
        began = time.monotonic()
        proc = subprocess.run(
            [args.bin, "inspect", args.db, "slice", "--file", path, "--line", str(line),
             "--col", str(col), "--name", name, "--format", "json",
             "--up-depth", str(args.up_depth), "--down-depth", str(args.down_depth)],
            text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        secs = time.monotonic() - began
        if proc.returncode != 0:
            print(f"skip  {label} {Path(path).name}:{line}: {proc.stderr.strip()}")
            continue
        s = json.loads(proc.stdout)
        r = {
            "field": label,
            "at": f"{Path(path).name}:{line}",
            "nodes": len(s["nodes"]),
            "edges": len(s["edges"]),
            "cross": sum(e["cross_context"] for e in s["edges"]),
            "contexts": len(s["contexts"]),
            "truncated": s["truncated_up"] or s["truncated_down"],
            "secs": secs,
        }
        results.append(r)
        print(f"  {r['nodes']:5} nodes {r['edges']:5} edges {r['cross']:4} cross "
              f"{r['contexts']:4} ctx {'T' if r['truncated'] else ' '} {secs:5.2f}s  "
              f"{label} @{r['at']}")
    if not results:
        return 1

    def dist(key):
        values = sorted(r[key] for r in results)
        return (f"median {statistics.median(values):g}, "
                f"p90 {values[int(0.9 * (len(values) - 1))]:g}, max {values[-1]:g}")

    print(f"\n{len(results)} slices from {total} written fields")
    for key in ("nodes", "edges", "cross", "contexts", "secs"):
        print(f"  {key}: {dist(key)}")
    flagged = sum(r["cross"] > 0 for r in results)
    truncated = sum(r["truncated"] for r in results)
    print(f"  with a cross-context edge: {flagged}; truncated: {truncated}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
