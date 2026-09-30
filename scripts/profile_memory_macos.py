#!/usr/bin/env python3
"""Per-phase memory and CPU sampler for `trace analyze` on macOS.

The Linux observer (`profile_memory.py`) reads glibc counters; this one is
the native macOS counterpart. It spawns the analyzer, polls
`proc_pid_rusage(RUSAGE_INFO_V4)` every 100 ms — resident size,
`phys_footprint`, the kernel's lifetime maximum footprint, user+system CPU —
tags each sample with the pipeline phase read from the analyzer's stderr
(`include-graph:`, `preprocess:`, `parse:`, `pch-done:`, `index:`,
`analyze:`, `export:`), and records `ru_maxrss` and CPU times from `wait4`
at exit. Footprint, not RSS, is the number to compare on macOS: under memory
pressure the kernel compresses pages, which leave RSS but stay in the
footprint (see docs/PERFORMANCE_REVIEW.md, "Incremental per-TU IR cache").

    python3 scripts/profile_memory_macos.py --label base --out /tmp/mem -- \\
        target/release/trace analyze ~/ability_ability_runtime --jobs 8 \\
        -o /tmp/mem/base.db

Writes `<out>/<label>.json` (all samples) and `<out>/<label>.log` (stderr
with timestamps), prints the child's pid at launch (for `sample <pid> ...`,
which may need `sudo`) and a per-phase summary at exit. Compare only within
one platform, allocator and corpus revision.
"""
import argparse
import ctypes
import json
import os
import resource
import subprocess
import sys
import threading
import time

if sys.platform != "darwin":
    sys.exit("error: scripts/profile_memory_macos.py requires macOS "
             "(use scripts/profile_memory.py on Linux/glibc)")

libproc = ctypes.CDLL("/usr/lib/libproc.dylib")
libproc.proc_pid_rusage.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_void_p]
libproc.proc_pid_rusage.restype = ctypes.c_int
RUSAGE_INFO_V4 = 4


class RusageInfoV4(ctypes.Structure):
    """`struct rusage_info_v4` from <sys/resource.h>."""
    _fields_ = [("ri_uuid", ctypes.c_uint8 * 16)] + [
        (name, ctypes.c_uint64) for name in (
            "ri_user_time", "ri_system_time", "ri_pkg_idle_wkups",
            "ri_interrupt_wkups", "ri_pageins", "ri_wired_size",
            "ri_resident_size", "ri_phys_footprint", "ri_proc_start_abstime",
            "ri_proc_exit_abstime",
            # v1
            "ri_child_user_time", "ri_child_system_time",
            "ri_child_pkg_idle_wkups", "ri_child_interrupt_wkups",
            "ri_child_pageins", "ri_child_elapsed_abstime",
            # v2
            "ri_diskio_bytesread", "ri_diskio_byteswritten",
            # v3
            "ri_cpu_time_qos_default", "ri_cpu_time_qos_maintenance",
            "ri_cpu_time_qos_background", "ri_cpu_time_qos_utility",
            "ri_cpu_time_qos_legacy", "ri_cpu_time_qos_user_initiated",
            "ri_cpu_time_qos_user_interactive", "ri_billed_system_time",
            "ri_serviced_system_time",
            # v4
            "ri_logical_writes", "ri_lifetime_max_phys_footprint",
            "ri_instructions", "ri_cycles", "ri_billed_energy",
            "ri_serviced_energy", "ri_interval_max_phys_footprint",
            "ri_runnable_time",
        )
    ]


class TimebaseInfo(ctypes.Structure):
    _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]


_tb = TimebaseInfo()
ctypes.CDLL("/usr/lib/libSystem.dylib").mach_timebase_info(ctypes.byref(_tb))
TICK_NS = _tb.numer / _tb.denom
_info = RusageInfoV4()


def rusage(pid):
    """(resident, footprint, lifetime max footprint, cpu seconds), or None
    once the process is gone. A terminated child stays answerable until it
    is reaped, so the final sample can be taken before `poll()`."""
    if libproc.proc_pid_rusage(pid, RUSAGE_INFO_V4, ctypes.byref(_info)) != 0:
        return None
    cpu = (_info.ri_user_time + _info.ri_system_time) * TICK_NS / 1e9
    return (_info.ri_resident_size, _info.ri_phys_footprint,
            _info.ri_lifetime_max_phys_footprint, cpu)


# Phase starts, keyed by the stderr line prefix that opens them.
PHASE_STARTS = [
    ("include-graph:", "warm"),
    ("preprocess:", "preprocess"),
    ("parse:", "pch"),
    ("pch-done:", "tu-merge"),
    ("index:", "analyze"),
    ("analyze:", "export"),
    ("export:", "done"),
]
PHASE_ORDER = ["graph", "warm", "preprocess", "pch", "tu-merge", "analyze", "export", "done"]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--label", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--interval", type=float, default=0.1)
    ap.add_argument("cmd", nargs=argparse.REMAINDER)
    args = ap.parse_args()
    cmd = args.cmd[1:] if args.cmd and args.cmd[0] == "--" else args.cmd
    if not cmd:
        ap.error("command to profile must be specified after '--'")
    os.makedirs(args.out, exist_ok=True)

    phase = "graph"
    phase_lock = threading.Lock()
    log_lines = []
    t0 = time.monotonic()
    proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    print(f"[{args.label}] pid={proc.pid}", flush=True)

    def reader():
        nonlocal phase
        for line in proc.stderr:
            now = time.monotonic() - t0
            log_lines.append((now, line.rstrip("\n")))
            for prefix, next_phase in PHASE_STARTS:
                if line.startswith(prefix):
                    with phase_lock:
                        phase = next_phase

    th = threading.Thread(target=reader, daemon=True)
    th.start()

    samples = []
    # Sample before reaping: the last sample then carries the exit state,
    # including the lifetime peak, even for a command shorter than one
    # interval.
    while True:
        r = rusage(proc.pid)
        if r is not None:
            with phase_lock:
                p = phase
            samples.append((round(time.monotonic() - t0, 3), p, r[0], r[1], r[2], round(r[3], 3)))
        if proc.poll() is not None:
            break
        time.sleep(args.interval)
    th.join(timeout=5)
    ru = resource.getrusage(resource.RUSAGE_CHILDREN)
    wall = time.monotonic() - t0

    # Per phase: peaks over its samples, and wall/CPU measured between the
    # last sample of the previous phase and the last sample of this one, so
    # the interval that straddles a boundary is charged to the phase that
    # ends in it rather than dropped.
    per_phase = {}
    prev_t, prev_cpu = 0.0, 0.0
    for t, p, rss, fp, _, cpu in samples:
        d = per_phase.get(p)
        if d is None:
            d = per_phase[p] = {"peak_rss": 0, "peak_footprint": 0, "samples": 0,
                                "t_start": prev_t, "cpu_start": prev_cpu}
        d["peak_rss"] = max(d["peak_rss"], rss)
        d["peak_footprint"] = max(d["peak_footprint"], fp)
        d["samples"] += 1
        d["t_end"], d["cpu_end"], d["end_rss"], d["end_footprint"] = t, cpu, rss, fp
        prev_t, prev_cpu = t, cpu
    result = {
        "label": args.label,
        "cmd": cmd,
        "pid": proc.pid,
        "exit": proc.returncode,
        "wall_s": round(wall, 2),
        "user_s": round(ru.ru_utime, 2),
        "sys_s": round(ru.ru_stime, 2),
        "ru_maxrss_bytes": ru.ru_maxrss,
        "lifetime_max_footprint_bytes": max((s[4] for s in samples), default=0),
        "sampled_peak_rss_bytes": max((s[2] for s in samples), default=0),
        "sampled_peak_footprint_bytes": max((s[3] for s in samples), default=0),
        "per_phase": per_phase,
        "phase_lines": [(t, l) for t, l in log_lines
                        if any(l.startswith(pfx) for pfx, _ in PHASE_STARTS)
                        or l.startswith(("discover:", "preprocess-done", "analysis complete"))],
        "samples": samples,
    }
    with open(os.path.join(args.out, f"{args.label}.json"), "w") as f:
        json.dump(result, f)
    with open(os.path.join(args.out, f"{args.label}.log"), "w") as f:
        for t, l in log_lines:
            f.write(f"{t:8.2f} {l}\n")

    mib = lambda b: f"{b / 2**20:,.0f}"
    print(f"[{args.label}] exit={proc.returncode} wall={wall:.1f}s user={ru.ru_utime:.1f}s sys={ru.ru_stime:.1f}s "
          f"ru_maxrss={mib(ru.ru_maxrss)} MiB lifetime_max_footprint={mib(result['lifetime_max_footprint_bytes'])} MiB")
    for p in PHASE_ORDER:
        if p in per_phase:
            d = per_phase[p]
            wall_p = d["t_end"] - d["t_start"]
            cpu_p = d["cpu_end"] - d["cpu_start"]
            par = f"{cpu_p / wall_p:4.1f}" if wall_p > 0 else "  --"
            print(f"  {p:10s} peak rss {mib(d['peak_rss']):>6} MiB  peak footprint {mib(d['peak_footprint']):>6} MiB  "
                  f"end rss {mib(d['end_rss']):>6} MiB  end footprint {mib(d['end_footprint']):>6} MiB  "
                  f"wall {wall_p:5.1f}s cpu {cpu_p:6.1f}s = {par} cores  ({d['samples']} samples)")
    return proc.returncode


if __name__ == "__main__":
    sys.exit(main())
