#!/usr/bin/env python3
"""Regression checker for the trace pipeline on the production eval corpora.

Re-analyzes each corpus (HDF, hiview, camera) fresh with the release binary,
after verifying the checkout is clean and at the revision pinned in
eval_expected.json (corpus base: --corpus-base, $TRACE_CORPUS_BASE, or ~),
then asserts:

1. Global metrics (files, functions, call edges by resolution, arg-flow,
   diagnostics, dlsym PAG edges) against `eval_expected.json`.
   Correctness numbers (diagnostics, dlsym, indirect edges) must match EXACTLY;
   bulk totals (functions/edges/arg-flow) use a tolerance band because the
   parallel index drifts a little between runs.
2. Per-case dispatch-site checks: the number of distinct callee NAMES a hub /
   virtual / fn-ptr dispatch site resolves to (exact) -- these are the
   eval-report correctness invariants.
3. Production probes for the new C++ features (overload groups, template
   member calls) plus calibration probes (external-class sites that must stay
   unresolved rather than degrading into noise edges).

Exit 0 when every check passes; non-zero otherwise. Uses only the stdlib.

Usage:
    python3 scripts/fetch_corpora.py      # once: check the pinned corpus revisions out
    python3 scripts/eval_check.py [--bin target/release/trace] [--jobs 8]
        [--budget 800000] [--outdir /tmp/eval_check] [--expected scripts/eval_expected.json]
        [--corpus-base DIR] [--skip-rev-check] [--allow-dirty]
"""

import argparse
import json
import os
import sqlite3
import subprocess
import traceback
import sys
from pathlib import Path

FAILS = 0
CHECKS = 0
# Expectation mismatches (FAILS) and setup/execution errors are different
# outcomes and get different exit codes: 0 clean, 1 some expectation missed,
# 2 the run could not be trusted at all (corpus missing / at the wrong
# revision / dirty, or `trace analyze` failed). A baseline binary is *meant*
# to miss the current expectations, so tooling that compares two binaries
# tolerates 1 on that side but must still abort on 2.
SETUP_ERRORS = 0


def log(level, text):
    marker = {"ok": "  ok", "warn": " warn", "fail": "FAIL", "info": "  .."}[level]
    print(f"{marker}  {text}")


def run(cmd, cwd, env):
    proc = subprocess.run(
        cmd, cwd=str(cwd), env=env, text=True,
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=1800)
    if proc.returncode != 0:
        # cmd[0] is a resolved Path; joining it raw raised TypeError and
        # replaced the diagnostic with a traceback.
        printable = " ".join(str(part) for part in cmd)
        log("fail", f"command failed rc={proc.returncode}: {printable}\n{proc.stdout[-2000:]}")
    return proc


def corpus_base(explicit=None):
    return Path(os.path.expanduser(explicit or os.environ.get("TRACE_CORPUS_BASE", "~")))


def git_output(args, cwd):
    proc = subprocess.run(["git", *args], cwd=str(cwd), text=True,
                          stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    return proc.stdout.strip() if proc.returncode == 0 else None


def checkout_matches(root, spec, skip_rev_check=False, allow_dirty=False):
    """The checkout under `root` must be at the pinned `rev` with a clean
    worktree (analysis discovers files from the worktree, so edits or
    untracked sources change the counts as much as a different revision).
    Returns whether the corpus should be analyzed; a problem counts as a
    failure unless the matching override downgrades it to a warning."""
    global FAILS, CHECKS, SETUP_ERRORS
    CHECKS += 1
    if not root.is_dir():
        log("fail", f"{spec['name']}: {root} is missing (run scripts/fetch_corpora.py)")
        SETUP_ERRORS += 1
        return False
    problems = []
    head = git_output(["rev-parse", "HEAD"], root)
    if head != spec["rev"]:
        problems.append((skip_rev_check,
                         f"at {head or 'no git HEAD'}, pinned {spec['rev']} "
                         f"(scripts/fetch_corpora.py --update)"))
    status = git_output(["status", "--porcelain"], root)
    if status:
        problems.append((allow_dirty,
                         f"worktree has local changes ({len(status.splitlines())} paths "
                         f"in `git status --porcelain`)"))
    if not problems:
        log("ok", f"{spec['name']}: clean checkout at pinned {spec['rev'][:12]}")
        return True
    proceed = all(overridden for overridden, _ in problems)
    if not proceed:
        SETUP_ERRORS += 1
    for overridden, text in problems:
        log("warn" if overridden else "fail", f"{spec['name']}: {root} {text}"
            + (" — analyzing anyway" if overridden else ""))
    return proceed


def analyze(trace_bin, corpus, expected, root, outdir, env):
    db = Path(outdir) / f"eval_check_{corpus}.db"
    log("info", f"analyzing {expected['name']} -> {db}")
    proc = run(
        [trace_bin, "analyze", str(root), "-o", str(db), "--jobs", str(expected.get("jobs", 8))],
        root, env)
    if proc.returncode != 0 or not db.exists():
        global SETUP_ERRORS
        log("fail", f"{corpus}: analyze produced no DB")
        SETUP_ERRORS += 1
        return None
    return db


def globals_from_db(db):
    con = sqlite3.connect(str(db))
    cur = con.cursor()
    out = {}
    out["files"] = cur.execute("SELECT COUNT(*) FROM files").fetchone()[0]
    out["functions_total"], out["functions_defined"], out["functions_external"] = cur.execute(
        "SELECT COUNT(*), SUM(is_defined), SUM(1 - is_defined) FROM functions").fetchone()
    edges = dict(cur.execute(
        "SELECT resolution, COUNT(*) FROM call_edges GROUP BY resolution").fetchall())
    out["edges_total"] = sum(edges.values())
    out["edges_direct"] = edges.get("direct", 0)
    out["edges_indirect"] = edges.get("indirect", 0)
    out["edges_external"] = edges.get("external", 0)
    out["edges_ipc"] = edges.get("ipc", 0)
    out["edges_unknown"] = {k: v for k, v in edges.items()
                            if k not in ("direct", "indirect", "external", "ipc")}
    out["arg_flow_edges"] = cur.execute("SELECT COUNT(*) FROM arg_flow_edges").fetchone()[0]
    out["diagnostics"] = cur.execute("SELECT COUNT(*) FROM diagnostics").fetchone()[0]
    out["dlsym_edges"] = cur.execute(
        "SELECT COUNT(*) FROM flow_edges WHERE kind LIKE '%dlsym%'").fetchone()[0]
    con.close()
    return out


def check_global(name, got, spec):
    global FAILS, CHECKS
    CHECKS += 1
    want = spec["value"]
    if spec["cmp"] == "exact":
        ok = got == want
        log("ok" if ok else "fail",
            f"{name}: {got} (expected exactly {want})")
    elif spec["cmp"] == "band":
        ok = abs(got - want) <= spec["tol"]
        log("ok" if ok else "fail",
            f"{name}: {got} (expected {want} +- {spec['tol']})")
    elif spec["cmp"] == "min":
        ok = got >= want
        log("ok" if ok else "fail",
            f"{name}: {got} (expected >= {want})")
    else:
        ok = False
        log("fail", f"{name}: unknown cmp {spec['cmp']}")
    if not ok:
        FAILS += 1


def check_site(db, site):
    global FAILS, CHECKS
    CHECKS += 1
    line_filter = "AND cs.line = :line" if site.get("line") else ""
    res_filter = "AND e.resolution = :res" if site.get("resolution") not in (None, "all") else ""
    file_filter = "AND f.path LIKE :file" if site.get("caller_file") else ""
    args = {"caller": site["caller"], "line": site.get("line", 0),
            "res": site.get("resolution", ""), "file": site.get("caller_file", "")}
    sql = f"""
        SELECT COUNT(DISTINCT tf.name)
        FROM call_sites cs
        JOIN call_edges e ON e.call_site_id = cs.id
        JOIN functions cf ON cf.id = cs.caller_fn_id
        JOIN files f ON f.id = cf.file_id
        JOIN functions tf ON tf.id = e.callee_fn_id
        WHERE cf.name LIKE :caller {line_filter} {res_filter} {file_filter}"""
    con = sqlite3.connect(str(db))
    got = con.execute(sql, args).fetchone()[0]
    con.close()
    ok = got == site["expected"]
    log("ok" if ok else "fail",
        f"{site['case']}: {got} targets (expected {site['expected']})"
        + (f" at line {site['line']}" if site.get("line") else ""))
    if not ok:
        FAILS += 1


def check_probe(db, probe):
    global FAILS, CHECKS
    CHECKS += 1
    con = sqlite3.connect(str(db))
    got = con.execute(probe["sql"]).fetchone()[0]
    con.close()
    want = probe["value"]
    if probe["cmp"] == "exact":
        ok = got == want
        log("ok" if ok else "fail", f"{probe['name']}: {got} (expected exactly {want})")
    elif probe["cmp"] == "band":
        ok = abs(got - want) <= probe["tol"]
        log("ok" if ok else "fail",
            f"{probe['name']}: {got} (expected {want} +- {probe['tol']})")
    elif probe["cmp"] == "min":
        ok = got >= want
        log("ok" if ok else "fail", f"{probe['name']}: {got} (expected >= {want})")
    else:
        ok = False
        log("fail", f"{probe['name']}: unknown cmp {probe['cmp']}")
    if not ok:
        FAILS += 1


def main():
    global FAILS, CHECKS
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default="target/release/trace")
    ap.add_argument("--jobs", type=int, default=8)
    ap.add_argument("--budget", type=int, default=800000)
    ap.add_argument("--outdir", default="/tmp/eval_check")
    ap.add_argument("--expected", default="scripts/eval_expected.json")
    ap.add_argument("--corpus-base",
                    help="directory holding the corpus checkouts (default $TRACE_CORPUS_BASE or ~)")
    ap.add_argument("--skip-rev-check", action="store_true",
                    help="warn instead of fail when a checkout is not at the pinned revision")
    ap.add_argument("--allow-dirty", action="store_true",
                    help="warn instead of fail when a checkout has local changes or untracked files")
    ap.add_argument("corpora", nargs="*",
                    help="subset of {hdf,hiview,camera}; default all")
    args = ap.parse_args()

    trace_bin = Path(os.path.expanduser(args.bin)).resolve()
    if not trace_bin.exists():
        log("fail", f"binary not found: {trace_bin} (build with cargo build --release)")
        sys.exit(2)

    with open(args.expected) as fh:
        expected = json.load(fh)

    outdir = Path(args.outdir)
    outdir.mkdir(parents=True, exist_ok=True)

    env = dict(os.environ)
    env["TRACE_SOLVE_BUDGET_POPS"] = str(expected.get("budget_pops", args.budget))

    corpus_names = args.corpora or list(expected["corpora"].keys())
    for corpus in corpus_names:
        spec = expected["corpora"].get(corpus)
        if spec is None:
            log("fail", f"unknown corpus {corpus!r} (have {list(expected['corpora'])})")
            sys.exit(2)
        root = corpus_base(args.corpus_base) / spec["dir"]
        if not checkout_matches(root, spec, args.skip_rev_check, args.allow_dirty):
            continue
        db = analyze(trace_bin, corpus, spec, root, outdir, env)
        if db is None:
            continue
        log("info", f"== {spec['name']}")
        got = globals_from_db(db)
        if got["edges_unknown"]:
            log("warn", f"unexpected edge resolutions: {got['edges_unknown']}")
        for key, s in spec["globals"].items():
            if key not in got:
                log("fail", f"missing global {key}")
                FAILS += 1
                continue
            check_global(key, got[key], s)
        for site in spec.get("sites", []):
            check_site(db, site)
        for probe in spec.get("probes", []):
            check_probe(db, probe)

    log("info", "")
    if SETUP_ERRORS:
        log("info", f"ERROR: {CHECKS} checks, {FAILS} failures, "
                    f"{SETUP_ERRORS} setup/execution errors — results are not usable")
        sys.exit(2)
    log("info", f"{'PASS' if FAILS == 0 else 'FAIL'}: {CHECKS} checks, {FAILS} failures")
    sys.exit(1 if FAILS else 0)


if __name__ == "__main__":
    try:
        main()
    except Exception:
        # Anything unexpected -- a `trace analyze` timeout, a SQLite error
        # reading a generated DB, malformed expectation JSON -- would
        # otherwise leave Python's default exit 1, which is the code that
        # means "an expectation was missed" and that a baseline comparison
        # deliberately tolerates. A crash is never a usable result, so it
        # exits 2 like the other unusable-run cases. `sys.exit` raises
        # SystemExit (not an Exception), so the real exit codes pass through.
        traceback.print_exc()
        log("fail", "unexpected error; the run produced no usable result")
        sys.exit(2)