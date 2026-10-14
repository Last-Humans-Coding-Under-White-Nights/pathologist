#!/usr/bin/env python3
"""Check inspect diagnostics against SQLite and summarize files to investigate.

Build the release CLI and run eval_check.py first, then pass its databases:
  python3 scripts/eval_diagnostics.py /tmp/eval_check/eval_check_*.db
The JSON report goes to stdout. Databases are opened read-only. Timings include
CLI startup and output capture, not analysis; no timing thresholds are enforced.
"""

import argparse
from collections import Counter
import json
from pathlib import Path
import sqlite3
import statistics
import subprocess
import time


def evaluate(binary, db, runs):
    with sqlite3.connect(db.as_uri() + "?mode=ro", uri=True) as conn:
        conn.row_factory = sqlite3.Row
        rows = [dict(row) for row in conn.execute(
            "SELECT d.severity, f.path AS file, d.line, d.message, d.stage "
            "FROM diagnostics d LEFT JOIN files f ON f.id=d.file_id ORDER BY d.id"
        )]

    cases = [("all", {})]
    cases += [(f"stage:{stage}", {"stage": stage})
              for stage in sorted({r["stage"] for r in rows})]
    cases += [(f"severity:{severity}", {"severity": severity})
              for severity in ("error", "warning", "info")]
    top_files = Counter(r["file"] for r in rows if r["file"]).most_common(5)
    if top_files:
        file = top_files[0][0]
        cases.append(("top-file", {"file": file}))
        first = next(r for r in rows if r["file"] == file)
        cases.append(("combined", {k: first[k] for k in ("file", "stage", "severity")}))
    absent = "__trace_eval_absent_stage__"
    while any(r["stage"] == absent for r in rows):
        absent += "_"
    cases.append(("empty", {"stage": absent}))

    results = []
    for name, filters in cases:
        expected = [r for r in rows if all(
            (r[k] is not None and v in r[k]) if k == "file" else r[k] == v
            for k, v in filters.items()
        )]
        args = [str(binary), "inspect", str(db), "diagnostics"]
        for key, value in filters.items():
            args += [f"--{key}", value]
        for fmt in ("text", "json"):
            samples = []
            previous = None
            for _ in range(runs):
                started = time.perf_counter()
                proc = subprocess.run(args + ["--format", fmt], check=True,
                                      capture_output=True, text=True, timeout=120)
                samples.append((time.perf_counter() - started) * 1000)
                if fmt == "json":
                    if json.loads(proc.stdout) != expected:
                        raise RuntimeError(f"{db}: {name}: JSON differs from SQLite")
                else:
                    want = "".join(
                        (f"{r['file']}:{r['line']}: " if r["file"] is not None else "")
                        + f"{r['severity']} [{r['stage']}]: {r['message']}\n"
                        for r in expected
                    ) or "No matching diagnostics.\n"
                    if proc.stdout != want:
                        raise RuntimeError(f"{db}: {name}: text differs from SQLite")
                output = (proc.stdout, proc.stderr)
                if previous is not None and output != previous:
                    raise RuntimeError(f"{db}: {name}: unstable output")
                previous = output
            results.append({"case": name, "format": fmt, "rows": len(expected),
                            "median_ms": round(statistics.median(samples), 3),
                            "min_ms": round(min(samples), 3),
                            "max_ms": round(max(samples), 3)})

    groups = Counter((r["stage"], r["severity"]) for r in rows)
    return {
        "database": str(db), "diagnostics": len(rows),
        "groups": [{"stage": stage, "severity": severity, "count": count}
                   for (stage, severity), count in sorted(groups.items())],
        "top_files": [{"file": file, "count": count} for file, count in top_files],
        "unlocated": sum(r["file"] is None for r in rows),
        "cases": results,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin", type=Path, default=Path("target/release/trace"))
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("databases", type=Path, nargs="+")
    args = parser.parse_args()
    if args.runs < 1:
        parser.error("--runs must be positive")
    binary = args.bin.resolve(strict=True)
    report = {"binary": str(binary), "version": subprocess.check_output(
        [str(binary), "--version"], text=True).strip(), "runs": args.runs,
        "results": [evaluate(binary, db.resolve(strict=True), args.runs)
                    for db in args.databases]}
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
