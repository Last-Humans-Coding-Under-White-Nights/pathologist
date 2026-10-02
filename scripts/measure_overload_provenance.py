#!/usr/bin/env python3
"""Compare repeated internal-overload provenance using two fresh release builds.

macOS only; uses profile_memory_macos.py, alternates binaries, and compares
every table's rows in insertion order, excluding analysis_run.created_at and
trace_version (build/run metadata).
All sources, databases and observer logs live in a TemporaryDirectory.
"""
import argparse
from contextlib import closing
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import platform
import sqlite3
import statistics
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parent.parent


def analysis_rows(path):
    with closing(sqlite3.connect(path)) as conn:
        tables = [r[0] for r in conn.execute(
            "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")]
        result = {}
        for table in tables:
            columns = [r[1] for r in conn.execute(f'PRAGMA table_info("{table}")')
                       if not (table == "analysis_run" and r[1] in ["created_at", "trace_version"])]
            fields = ",".join(f'"{c}"' for c in columns)
            result[table] = list(conn.execute(f'SELECT {fields} FROM "{table}" ORDER BY rowid'))
        return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline-binary", type=Path, required=True)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release/trace")
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.runs < 2:
        parser.error("--runs must be at least 2")
    binaries = {"baseline": args.baseline_binary.resolve(), "fixed": args.binary.resolve()}
    result = {
        "measured_at": datetime.now(timezone.utc).isoformat(),
        "platform": platform.platform(),
        "cpu_and_memory": subprocess.check_output(
            ["sysctl", "-n", "machdep.cpu.brand_string", "hw.memsize"], text=True).splitlines(),
        "rustc": subprocess.check_output(["rustc", "-Vv"], text=True),
        "binaries": {name: {"path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
                     for name, path in binaries.items()},
        "runs": args.runs, "interval_s": 0.001, "cases": [],
    }
    with tempfile.TemporaryDirectory(prefix="trace-overload-measure-") as scratch:
        work = Path(scratch)
        source = work / "source"
        source.mkdir()
        for n in [500, 1000, 2000]:
            (source / "main.cpp").write_text(
                "static void cb(int) {}\nstatic void cb(double) {}\n"
                "void (*fp)(int);\nvoid writes() {\n" + "    fp = cb;\n" * n + "}\n")
            for full in [False, True]:
                case = {"n": n, "export": "full" if full else "minimal",
                        "samples": {name: [] for name in binaries}}
                expected = None
                db = work / "analysis.db"
                for run in range(args.runs):
                    order = ["baseline", "fixed"] if run % 2 == 0 else ["fixed", "baseline"]
                    for name in order:
                        label = f'{name}-{n}-{case["export"]}-{run}'
                        command = [str(binaries[name]), "analyze", str(source), "--jobs", "1",
                                   "-o", str(db)] + (["--full-export"] if full else [])
                        subprocess.run([sys.executable, str(ROOT / "scripts/profile_memory_macos.py"),
                                        "--label", label, "--out", str(work), "--interval", "0.001",
                                        "--", *command], check=True, stdout=subprocess.DEVNULL)
                        data = json.loads((work / f"{label}.json").read_text())
                        if data["exit"] != 0 or data["lifetime_max_footprint_bytes"] == 0:
                            raise RuntimeError(f"invalid measurement: {label}")
                        case["samples"][name].append({key: data[key] for key in [
                            "wall_s", "ru_maxrss_bytes", "lifetime_max_footprint_bytes",
                            "sampled_peak_footprint_bytes"]})
                        rows = analysis_rows(db)
                        if expected is None:
                            expected = rows
                        else:
                            for table in sorted(rows.keys() | expected.keys()):
                                assert rows.get(table) == expected.get(table), (
                                    f"analysis rows or insertion order differ: {label}, {table}: "
                                    f"{rows.get(table, [])[:1]} != {expected.get(table, [])[:1]}")
                case["identical_analysis_rows"] = True
                case["flow_edges"] = len(expected["flow_edges"])
                case["flow_origins"] = len(expected["flow_origins"])
                case["summary"] = {
                    name: {key: {"median": statistics.median(s[key] for s in samples),
                                 "min": min(s[key] for s in samples), "max": max(s[key] for s in samples)}
                           for key in samples[0]}
                    for name, samples in case["samples"].items()}
                result["cases"].append(case)
                print(json.dumps(case), flush=True)
    args.output.write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
