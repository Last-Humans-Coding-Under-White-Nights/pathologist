#!/usr/bin/env python3
"""Linux/glibc stage memory snapshots, with optional Massif allocation stacks."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("target", type=Path)
    parser.add_argument("--jobs", type=int, default=os.cpu_count() or 1)
    parser.add_argument("--outdir", type=Path, default=Path("/tmp/trace-memory-profile"))
    parser.add_argument("--trim", action="store_true", help="diagnostic malloc_trim at PCH/index/analyze/export boundaries")
    parser.add_argument("--massif", action="store_true", help="also run Valgrind Massif (much slower; profiler timings are not benchmarks)")
    args = parser.parse_args()
    if sys.platform != "linux":
        parser.error("stage snapshots require Linux and glibc")
    if args.jobs < 1:
        parser.error("--jobs must be positive")
    root = Path(__file__).resolve().parent.parent
    outdir = args.outdir.resolve()
    outdir.mkdir(parents=True, exist_ok=True)
    subprocess.run(["cargo", "build", "-p", "trace-cli", "--release"], cwd=root, check=True)
    binary = root / "target/release/trace"
    command = [str(binary), "analyze", str(args.target.resolve()), "--jobs", str(args.jobs)]
    log_path = outdir / "native.log"
    with tempfile.TemporaryDirectory(prefix="trace-memory-observer-") as scratch:
        library = Path(scratch) / "observer.so"
        subprocess.run([os.environ.get("CC", "cc"), "-O2", "-Wall", "-Wextra", "-shared", "-fPIC",
                        str(root / "scripts/memory_observer.c"), "-o", str(library)], check=True)
        env = os.environ.copy()
        previous = env.get("LD_PRELOAD", "")
        env["LD_PRELOAD"] = str(library) + (":" + previous if previous else "")
        env.pop("TRACE_MEMORY_TRIM", None)
        if args.trim:
            env["TRACE_MEMORY_TRIM"] = "1"
        with log_path.open("w") as log:
            subprocess.run(["/usr/bin/time", "-v", *command, "-o", str(outdir / "native.db")],
                           env=env, stdout=log, stderr=log, check=True)
    rows = []
    for line in log_path.read_text().splitlines():
        match = re.match(r"\[memory\] stage=(\S+?):?\s+event=(\S+) (.*)", line)
        if match:
            values = dict(re.findall(r"(\w+)=(\d+)", match[3]))
            rows.append({"stage": match[1], "event": match[2],
                         **{k: int(v) for k, v in values.items() if k.endswith("_kib")}})
    if not rows:
        raise RuntimeError("no snapshots captured; see native.log (glibc/write interposition required)")
    (outdir / "stages.json").write_text(json.dumps(rows, indent=2) + "\n")
    print("Stage / event                  RSS MiB  Allocated MiB  Free arena MiB")
    for row in rows:
        print(f"{row['stage'] + '/' + row['event']:<30} {row['rss_kib']/1024:>7.1f} "
              f"{row['allocated_kib']/1024:>14.1f} {row['free_arena_kib']/1024:>15.1f}")
    if args.massif:
        with (outdir / "massif.log").open("w") as log:
            subprocess.run(["valgrind", "--tool=massif", f"--massif-out-file={outdir / 'massif.out'}",
                            "--time-unit=B", "--detailed-freq=1", "--max-snapshots=80", "--threshold=0.1",
                            *command, "-o", str(outdir / "massif.db")], stdout=log, stderr=log, check=True)
        with (outdir / "massif.txt").open("w") as output:
            subprocess.run(["ms_print", str(outdir / "massif.out")], stdout=output, check=True)
    print(f"Artifacts: {outdir}")


if __name__ == "__main__":
    main()
