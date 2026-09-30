#!/usr/bin/env python3
"""Per-phase memory and CPU sampler for `trace analyze` on Windows.

The Windows counterpart of `profile_memory.py` (Linux/glibc) and
`profile_memory_macos.py`. It spawns the analyzer, polls
`GetProcessMemoryInfo` every 100 ms — working set, private bytes (commit
charge, `PagefileUsage`) and the kernel's lifetime peaks of both
(`PeakWorkingSetSize`, `PeakPagefileUsage`) — and `GetProcessTimes`
(user+kernel CPU), tags each sample with the pipeline phase read from the
analyzer's stderr (`include-graph:`, `preprocess:`, `parse:`, `pch-done:`,
`index:`, `analyze:`, `export:`), and records the final CPU times at exit.
Peak working set is the number to compare on Windows (it is what the issue
tracker and Task Manager report); private bytes show what the heap has
committed, which a trimmed or decommitted heap lowers first.

    py -3 scripts\\profile_memory_windows.py --label base --out C:\\mem -- ^
        target\\release\\trace.exe analyze C:\\corpora\\multimedia_camera_framework ^
        --jobs 8 -o C:\\mem\\base.db

Writes `<out>/<label>.json` (all samples) and `<out>/<label>.log` (stderr
with timestamps), prints the child's pid at launch and a per-phase summary
at exit. Compare only within one platform, allocator and corpus revision.
"""
import argparse
import json
import os
import subprocess
import sys
import threading
import time

# Phase starts, keyed by the stderr line prefix that opens them (the same
# table as the macOS sampler).
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


def phase_after(phase, line):
    """The phase in effect after the analyzer printed `line` while in
    `phase`: a phase-opening line moves to its phase, any other line keeps
    it. The per-file `parse: 2/3 path` progress lines of `--jobs 1` are
    printed during the TU phase, after `pch-done:`, and are not openers."""
    for prefix, next_phase in PHASE_STARTS:
        if line.startswith(prefix) and not _is_progress_line(prefix, line):
            return next_phase
    return phase


def _is_progress_line(prefix, line):
    """`parse: 2/3 path`, as opposed to the opener `parse: N orphan headers, M TUs`."""
    head = line[len(prefix):].strip().split(" ", 1)[0]
    return "/" in head


def summarize(samples):
    """Per phase, over its `(t, phase, working_set, private, cpu)` samples:
    peak and end values, and the wall/CPU span measured between the last
    sample of the previous phase and the last sample of this one, so the
    interval that straddles a boundary is charged to the phase that ends in
    it rather than dropped."""
    per_phase = {}
    prev_t, prev_cpu = 0.0, 0.0
    for t, p, ws, private, cpu in samples:
        d = per_phase.get(p)
        if d is None:
            d = per_phase[p] = {"peak_working_set": 0, "peak_private": 0, "samples": 0,
                                "t_start": prev_t, "cpu_start": prev_cpu}
        d["peak_working_set"] = max(d["peak_working_set"], ws)
        d["peak_private"] = max(d["peak_private"], private)
        d["samples"] += 1
        d["t_end"], d["cpu_end"], d["end_working_set"], d["end_private"] = t, cpu, ws, private
        prev_t, prev_cpu = t, cpu
    return per_phase


class _Win32:
    """Lazily bound Win32 process counters, so this module imports (and its
    pure parts test) on any platform."""

    def __init__(self):
        import ctypes
        from ctypes import wintypes

        class ProcessMemoryCountersEx(ctypes.Structure):
            _fields_ = [
                ("cb", wintypes.DWORD),
                ("PageFaultCount", wintypes.DWORD),
                ("PeakWorkingSetSize", ctypes.c_size_t),
                ("WorkingSetSize", ctypes.c_size_t),
                ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
                ("QuotaPagedPoolUsage", ctypes.c_size_t),
                ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
                ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
                ("PagefileUsage", ctypes.c_size_t),
                ("PeakPagefileUsage", ctypes.c_size_t),
                ("PrivateUsage", ctypes.c_size_t),
            ]

        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        self._get_memory_info = kernel32.K32GetProcessMemoryInfo
        self._get_memory_info.argtypes = [wintypes.HANDLE, ctypes.POINTER(ProcessMemoryCountersEx), wintypes.DWORD]
        self._get_memory_info.restype = wintypes.BOOL
        self._get_process_times = kernel32.GetProcessTimes
        self._get_process_times.argtypes = [wintypes.HANDLE] + [ctypes.POINTER(wintypes.FILETIME)] * 4
        self._get_process_times.restype = wintypes.BOOL
        self._counters = ProcessMemoryCountersEx()
        self._counters.cb = ctypes.sizeof(ProcessMemoryCountersEx)
        self._times = [wintypes.FILETIME() for _ in range(4)]
        self._byref = ctypes.byref

    def memory(self, handle):
        """(working set, private bytes, peak working set, peak private bytes)
        or None when the counters are no longer readable."""
        c = self._counters
        if not self._get_memory_info(handle, self._byref(c), c.cb):
            return None
        return (c.WorkingSetSize, c.PagefileUsage, c.PeakWorkingSetSize, c.PeakPagefileUsage)

    def cpu(self, handle):
        """(user seconds, kernel seconds), readable until the handle closes,
        so after exit too."""
        creation, exit_, kernel, user = self._times
        if not self._get_process_times(handle, self._byref(creation), self._byref(exit_),
                                       self._byref(kernel), self._byref(user)):
            return None
        to_s = lambda ft: ((ft.dwHighDateTime << 32) | ft.dwLowDateTime) / 1e7
        return (to_s(user), to_s(kernel))


def main():
    if sys.platform != "win32":
        sys.exit("error: scripts/profile_memory_windows.py requires Windows "
                 "(use scripts/profile_memory.py on Linux/glibc, scripts/profile_memory_macos.py on macOS)")
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
    win32 = _Win32()

    phase = "graph"
    phase_lock = threading.Lock()
    log_lines = []
    t0 = time.monotonic()
    proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True,
                            encoding="utf-8", errors="replace")
    handle = proc._handle
    print(f"[{args.label}] pid={proc.pid}", flush=True)

    def reader():
        nonlocal phase
        for line in proc.stderr:
            now = time.monotonic() - t0
            line = line.rstrip("\n")
            log_lines.append((now, line))
            with phase_lock:
                phase = phase_after(phase, line)

    th = threading.Thread(target=reader, daemon=True)
    th.start()

    samples = []
    kernel_peak_ws = kernel_peak_private = 0
    # Sample before reaping, so the last sample carries the exit state even
    # for a command shorter than one interval.
    while True:
        m = win32.memory(handle)
        c = win32.cpu(handle)
        if m is not None and c is not None:
            with phase_lock:
                p = phase
            samples.append((round(time.monotonic() - t0, 3), p, m[0], m[1], round(c[0] + c[1], 3)))
            kernel_peak_ws = max(kernel_peak_ws, m[2])
            kernel_peak_private = max(kernel_peak_private, m[3])
        if proc.poll() is not None:
            break
        time.sleep(args.interval)
    th.join(timeout=5)
    wall = time.monotonic() - t0
    user_s, sys_s = win32.cpu(handle) or (0.0, 0.0)

    per_phase = summarize(samples)
    result = {
        "label": args.label,
        "cmd": cmd,
        "pid": proc.pid,
        "exit": proc.returncode,
        "wall_s": round(wall, 2),
        "user_s": round(user_s, 2),
        "sys_s": round(sys_s, 2),
        "peak_working_set_bytes": kernel_peak_ws,
        "peak_private_bytes": kernel_peak_private,
        "sampled_peak_working_set_bytes": max((s[2] for s in samples), default=0),
        "sampled_peak_private_bytes": max((s[3] for s in samples), default=0),
        "per_phase": per_phase,
        "phase_lines": [(t, l) for t, l in log_lines
                        if any(l.startswith(pfx) for pfx, _ in PHASE_STARTS)
                        or l.startswith(("discover:", "preprocess-done", "analysis complete", "heap:"))],
        "samples": samples,
    }
    with open(os.path.join(args.out, f"{args.label}.json"), "w", encoding="utf-8") as f:
        json.dump(result, f)
    with open(os.path.join(args.out, f"{args.label}.log"), "w", encoding="utf-8") as f:
        for t, l in log_lines:
            f.write(f"{t:8.2f} {l}\n")

    mib = lambda b: f"{b / 2**20:,.0f}"
    print(f"[{args.label}] exit={proc.returncode} wall={wall:.1f}s user={user_s:.1f}s sys={sys_s:.1f}s "
          f"peak_working_set={mib(kernel_peak_ws)} MiB peak_private={mib(kernel_peak_private)} MiB")
    for p in PHASE_ORDER:
        if p in per_phase:
            d = per_phase[p]
            wall_p = d["t_end"] - d["t_start"]
            cpu_p = d["cpu_end"] - d["cpu_start"]
            par = f"{cpu_p / wall_p:4.1f}" if wall_p > 0 else "  --"
            print(f"  {p:10s} peak ws {mib(d['peak_working_set']):>6} MiB  peak private {mib(d['peak_private']):>6} MiB  "
                  f"end ws {mib(d['end_working_set']):>6} MiB  end private {mib(d['end_private']):>6} MiB  "
                  f"wall {wall_p:5.1f}s cpu {cpu_p:6.1f}s = {par} cores  ({d['samples']} samples)")
    return proc.returncode


if __name__ == "__main__":
    sys.exit(main())
