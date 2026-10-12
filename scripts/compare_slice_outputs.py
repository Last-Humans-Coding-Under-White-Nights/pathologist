#!/usr/bin/env python3
"""Re-run deterministic field-site sampling and full slice-JSON comparisons.

  python3 scripts/compare_slice_outputs.py before.db after.db \
      --before-bin /path/to/baseline/trace --bin target/release/trace

Defaults to eight fields and four steps up/down, matching the compact export
evaluation. Requires the original source trees for position/name resolution.
Exit 1 indicates changed sample sites or JSON. Timings are individual samples.
"""
import argparse
import json
from pathlib import Path
import subprocess
import time

from slice_sizes import REPO_ROOT, slice_command, starts


def compare_slices(before, after, before_binary, binary, sample=8, up_depth=4, down_depth=4):
    before_count, before_sites = starts(before, sample)
    after_count, after_sites = starts(after, sample)
    # Storage rowids select sites but are not logical output identities.
    prior, current = [site[:3] for site in before_sites], [site[:3] for site in after_sites]
    equal_sites = (before_count, prior) == (after_count, current)
    report = {"equal": equal_sites, "equal_sites": equal_sites,
              "fields_before": before_count, "fields_after": after_count,
              "sites_before": prior, "sites_after": current, "slices": []}
    if not equal_sites:
        return report
    for site in before_sites:
        outputs, seconds = [], []
        for reader, db in [(before_binary, before), (binary, after)]:
            began = time.monotonic()
            proc = subprocess.run(slice_command(reader, db, site, up_depth, down_depth),
                                  capture_output=True, text=True, check=True, timeout=60)
            seconds.append(time.monotonic() - began)
            outputs.append(json.loads(proc.stdout))
        equal = outputs[0] == outputs[1]
        report["equal"] &= equal
        report["slices"].append({"field": site[0], "file": site[1], "line": site[2],
                                 "equal": equal, "seconds_before": seconds[0],
                                 "seconds_after": seconds[1]})
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before", type=Path)
    parser.add_argument("after", type=Path)
    parser.add_argument("--before-bin", type=Path, default=REPO_ROOT / "target/release/trace")
    parser.add_argument("--bin", type=Path, default=REPO_ROOT / "target/release/trace")
    parser.add_argument("--sample", type=int, default=8)
    parser.add_argument("--up-depth", type=int, default=4)
    parser.add_argument("--down-depth", type=int, default=4)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.sample < 1 or min(args.up_depth, args.down_depth) < 0:
        parser.error("sample must be positive and depths must be non-negative")
    report = compare_slices(args.before, args.after, args.before_bin, args.bin,
                            args.sample, args.up_depth, args.down_depth)
    output = json.dumps(report, indent=2) + "\n"
    if args.output:
        args.output.write_text(output)
    print(output, end="")
    return 0 if report["equal"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
