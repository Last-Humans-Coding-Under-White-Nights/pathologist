#!/usr/bin/env python3
"""Check repeated absent-receiver misses and record bounded synthetic scaling.
Build both release binaries first; compilation and database checks are untimed.
"""
import argparse
import json
import os
from pathlib import Path
import re
import sqlite3
import statistics
import subprocess
import tempfile
import time


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--baseline', required=True, type=Path)
    ap.add_argument('--candidate', required=True, type=Path)
    ap.add_argument('--sizes', nargs='+', type=int, default=[1000, 2000, 4000])
    ap.add_argument('--repeats', type=int, default=3)
    ap.add_argument('--output', type=Path)
    ap.add_argument('--max-growth', type=float, default=8.0, help='Maximum candidate wall-time growth between smallest/largest default sizes (non-CI check).')
    args = ap.parse_args()
    binaries = {'baseline': args.baseline.resolve(), 'candidate': args.candidate.resolve()}
    runs = []
    with tempfile.TemporaryDirectory() as scratch:
        scratch = Path(scratch)
        for size in args.sizes:
            root = scratch / f'source{size}'
            root.mkdir()
            text = '\n'.join(f'class Different{i} {{ public: void Ping() {{}} }};' for i in range(size))
            (root / 'main.cpp').write_text(text + '\nvoid use(Absent *p) {\n' + 'p->Ping();\n' * size + '}\n')
            for repeat in range(args.repeats):
                labels = ['baseline', 'candidate'] if repeat % 2 == 0 else ['candidate', 'baseline']
                for label in labels:
                    db = scratch / f'{size}-{repeat}-{label}.db'
                    start = time.perf_counter()
                    proc = subprocess.run([str(binaries[label]), 'analyze', str(root), '--jobs', '1', '-o', str(db)],
                                          env={**os.environ, 'TRACE_SOLVER_STATS': '1'}, text=True, capture_output=True, check=True)
                    elapsed = time.perf_counter() - start
                    with sqlite3.connect(db) as c:
                        edges = dict(c.execute('SELECT resolution, count(*) FROM call_edges GROUP BY resolution'))
                    assert edges == {'external': size}, (label, size, edges)
                    # Keep the solver's own timing in raw output as well as elapsed wall time.
                    resolve = [line for line in proc.stderr.splitlines() if 'RESOLVE' in line]
                    match = re.search(r'elapsed=([0-9.]+)(ns|µs|ms|s)', '\n'.join(resolve))
                    resolve_s = float(match[1]) * {'ns': 1e-9, 'µs': 1e-6, 'ms': 1e-3, 's': 1}[match[2]] if match else None
                    runs.append({'binary': label, 'size': size, 'repeat': repeat, 'wall_s': elapsed, 'resolve_s': resolve_s})
    summary = {label: {str(n): statistics.median(r['wall_s'] for r in runs if r['binary'] == label and r['size'] == n)
                       for n in args.sizes} for label in binaries}
    resolve_summary = {label: {str(n): statistics.median(r['resolve_s'] for r in runs if r['binary'] == label and r['size'] == n and r['resolve_s'] is not None) for n in args.sizes} for label in binaries}
    growth = summary['candidate'][str(max(args.sizes))] / summary['candidate'][str(min(args.sizes))]
    assert growth <= args.max_growth, (growth, args.max_growth)
    report = {'candidate_wall_growth': growth, 'median_resolve_s': resolve_summary, 'binaries': {k: str(v) for k, v in binaries.items()}, 'runs': runs, 'median_wall_s': summary}
    if args.output:
        args.output.write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(summary, indent=2))


if __name__ == '__main__':
    main()
