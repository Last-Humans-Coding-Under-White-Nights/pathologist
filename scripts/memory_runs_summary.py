#!/usr/bin/env python3
"""Summarize a directory of memory-sampler runs as one Markdown table.

Each run is a `<config>-<round>.json` written by `profile_memory_windows.py`
(or the macOS sampler's keys mapped to the same names) next to a
`<config>-<round>.digest` holding `db_digest.py --total` of its output
database. Runs group by configuration; the table lists every round's peak
working set, peak private bytes, wall and CPU, with the change of each
configuration's median against the baseline's. Every digest must be the
same: the configurations only change the allocator, never the analysis
(AGENTS.md, invariant 10), so a differing digest fails the run.

    python3 scripts/memory_runs_summary.py C:\\mem --baseline default >> $GITHUB_STEP_SUMMARY
"""
import argparse
import json
import re
import statistics
import sys
from pathlib import Path


class DigestMismatch(Exception):
    pass


LABEL = re.compile(r"^(?P<config>.+)-(?P<round>\d+)$")


def collect(out):
    """`{config: [run, …]}` in configuration-name order, each run its JSON
    result plus `digest`, rounds in numeric order. Raises `RuntimeError`
    for a run that exited non-zero and `DigestMismatch` when the digests
    differ."""
    runs = []
    for path in sorted(Path(out).glob("*.json")):
        run = json.loads(path.read_text(encoding="utf-8"))
        m = LABEL.match(path.stem)
        if not m:
            continue
        if run.get("exit", 1) != 0:
            raise RuntimeError(f"{path.stem}: analyzer exited with {run.get('exit')}")
        run["digest"] = path.with_suffix(".digest").read_text(encoding="utf-8").strip()
        runs.append((m.group("config"), int(m.group("round")), run))
    configs = {}
    for config, rnd, run in sorted(runs, key=lambda r: (r[0], r[1])):
        configs.setdefault(config, []).append(run)
    reference = runs[0][2]["digest"] if runs else None
    for config, rnd, run in runs:
        if run["digest"] != reference:
            raise DigestMismatch(
                f"{config}-{rnd}: database digest {run['digest'][:12]}… differs from "
                f"{runs[0][0]}-{runs[0][1]}'s {reference[:12]}…")
    return configs


def _mib(b):
    return f"{b / 2**20:,.0f}"


def _cell(values, fmt, base_median=None, unit=""):
    text = " / ".join(fmt(v) for v in values) + unit
    if base_median:
        delta = (statistics.median(values) - base_median) / base_median * 100
        sign = "+" if delta >= 0 else "−"
        text += f" ({sign}{abs(delta):.{0 if unit == ' MiB' else 1}f}%)"
    return text


def render(configs, baseline="default"):
    """The Markdown table, then the shared digest."""
    base = configs.get(baseline)
    medians = {}
    if base:
        for key in ("peak_working_set_bytes", "peak_private_bytes", "wall_s"):
            medians[key] = statistics.median(r[key] for r in base)
    lines = [
        "| Configuration | Peak working set | Peak private bytes | Wall | CPU (user+sys) |",
        "|---|---:|---:|---:|---:|",
    ]
    for config, runs in configs.items():
        is_base = config == baseline
        lines.append("| {} | {} | {} | {} | {} |".format(
            config,
            _cell([r["peak_working_set_bytes"] for r in runs], _mib,
                  None if is_base else medians.get("peak_working_set_bytes"), " MiB"),
            _cell([r["peak_private_bytes"] for r in runs], _mib,
                  None if is_base else medians.get("peak_private_bytes"), " MiB"),
            _cell([r["wall_s"] for r in runs], lambda v: f"{v:.1f}",
                  None if is_base else medians.get("wall_s"), " s"),
            _cell([r["user_s"] + r["sys_s"] for r in runs], lambda v: f"{v:.1f}", None, " s"),
        ))
    digest = next(iter(configs.values()))[0]["digest"] if configs else "(no runs)"
    lines.append("")
    lines.append(f"All runs' analysis tables identical: digest `{digest}`.")
    return "\n".join(lines)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("out", help="directory of <config>-<round>.json and .digest files")
    ap.add_argument("--baseline", default="default")
    args = ap.parse_args(argv)
    try:
        configs = collect(args.out)
    except (DigestMismatch, RuntimeError) as err:
        print(f"error: {err}", file=sys.stderr)
        return 1
    print(render(configs, baseline=args.baseline))
    return 0


if __name__ == "__main__":
    sys.exit(main())
