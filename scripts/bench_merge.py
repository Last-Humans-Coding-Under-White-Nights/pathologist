#!/usr/bin/env python3
"""Pair two merger binaries on fixed inputs and compare all logical output rows.

Build both release binaries first, with separate Cargo target directories.
Timed processes run serially, alternating order after one warm-up each. Elapsed time uses a monotonic clock; GNU time
supplies CPU and peak RSS. Retain commands, hashes, logs and output databases
in --outdir. Output comparison and hashing are outside the timed runs.
"""

import argparse
import hashlib
import json
from pathlib import Path
import shutil
import statistics
import subprocess
import time

from compare_analysis_databases import compare_databases


def sha256(path):
    with path.open("rb") as stream:
        digest = hashlib.sha256()
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
        return digest.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--outdir", type=Path, required=True)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("inputs", type=Path, nargs="+")
    args = parser.parse_args()
    if args.repeats < 1:
        parser.error("--repeats must be positive")
    timer = shutil.which("time")
    if timer is None:
        parser.error("GNU time is required for CPU and RSS measurements")
    binaries = {"baseline": args.baseline.resolve(), "candidate": args.candidate.resolve()}
    inputs = [path.resolve() for path in args.inputs]
    outdir = args.outdir.resolve()
    outdir.mkdir(parents=True, exist_ok=True)
    outputs = {label: outdir / f"{label}.db" for label in binaries}
    if any(output in inputs for output in outputs.values()):
        parser.error("benchmark outputs must not overwrite input databases")
    setup = {
        "binaries": {label: {"path": str(path), "sha256": sha256(path)}
                     for label, path in binaries.items()},
        "inputs": [{"path": str(path), "sha256": sha256(path)} for path in inputs],
        "repeats": args.repeats,
        "wall_clock": "time.perf_counter (monotonic)",
    }
    (outdir / "setup.json").write_text(json.dumps(setup, indent=2) + "\n")
    runs = []

    def measure(label, repeat):
        stem = outdir / f"{label}-{repeat}"
        command = [str(binaries[label]), *map(str, inputs), "-o", str(outputs[label])]
        with stem.with_suffix(".log").open("w") as log:
            start = time.perf_counter()
            subprocess.run([timer, "-f", "%U %S %M", "-o",
                            str(stem.with_suffix(".time")), *command],
                           stdout=log, stderr=subprocess.STDOUT, check=True)
            elapsed = time.perf_counter() - start
        user, system, rss = map(float, stem.with_suffix(".time").read_text().split())
        result = {"binary": label, "repeat": repeat, "command": command,
                  "wall_s": elapsed, "cpu_s": user + system,
                  "rss_mib": rss / 1024, "output_bytes": outputs[label].stat().st_size}
        stem.with_suffix(".json").write_text(json.dumps(result, indent=2) + "\n")
        print(f"{label} {repeat}: {elapsed:.3f} s, CPU {user + system:.3f} s, "
              f"RSS {rss / 1024:.2f} MiB", flush=True)
        return result

    for label in binaries:
        measure(label, "warmup")
    for repeat in range(args.repeats):
        order = list(binaries) if repeat % 2 == 0 else list(reversed(binaries))
        for label in order:
            runs.append(measure(label, repeat))
            (outdir / "results.json").write_text(json.dumps(runs, indent=2) + "\n")
    summary = {}
    for label in binaries:
        samples = [row for row in runs if row["binary"] == label]
        summary[label] = {key: statistics.median(row[key] for row in samples)
                          for key in ("wall_s", "cpu_s", "rss_mib", "output_bytes")}
        summary[label]["wall_range"] = [min(row["wall_s"] for row in samples),
                                        max(row["wall_s"] for row in samples)]
    (outdir / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    comparison = compare_databases(outputs["baseline"], outputs["candidate"])
    (outdir / "comparison.json").write_text(json.dumps(comparison, indent=2, default=repr) + "\n")
    print(json.dumps(summary, indent=2), flush=True)
    if not comparison["equal"]:
        different = [name for name, result in comparison["relations"].items()
                     if not result["equal"]]
        raise RuntimeError(f"merged output differs: {different}")
    print(f"All {len(comparison['relations'])} logical output projections match.")


if __name__ == "__main__":
    main()
