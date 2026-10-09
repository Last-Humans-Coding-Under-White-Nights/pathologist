#!/usr/bin/env python3
"""Retro-test of `trace inspect slice` (#205) on three camera race fixes.

Re-runs the check of docs/EVAL_REPORT.md ("Value slice: #205") from the
inputs in scripts/cve_slice_retro.json: for each fix, the camera tree before
it (`pre`, the merge's first parent) and after it (`post`, the merge), the
blob hashes of the files it patched, and the member it protected. Each tree
is analysed, the slice is taken from the member, and every expected memory
edge (kind, function, line of its pattern) is looked up in it:
`cross_context` must be as recorded, and `present: false` sites must be
absent. Locks are not modelled, so pre and post expect the same flags.

The camera history is fetched as a blobless bare clone into --work (default:
$TMPDIR/trace_cve_retro), with one worktree per revision; nothing touches
the eval corpus checkout. A tree's database is kept there under the hash
of the binary that built it, so a rebuilt binary analyses afresh. Uses only
the stdlib.

Usage:
    python3 scripts/cve_slice_retro.py [--bin target/release/trace]
        [--work DIR] [--inputs scripts/cve_slice_retro.json] [--revisions pre,post]

Exit 0 when every expectation holds, 1 when one does not, 2 when a tree
could not be fetched, verified or analysed.
"""

import argparse
import hashlib
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent


def git(args, cwd=None, check=True):
    proc = subprocess.run(["git", *args], cwd=cwd, text=True,
                          stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    if check and proc.returncode != 0:
        raise RuntimeError(f"git {' '.join(args)} failed:\n{proc.stdout}")
    return proc.stdout.strip()


def ensure_tree(work, repo, rev):
    """A worktree of the blobless clone at `rev`, created on first use."""
    bare = work / "camera.git"
    if not bare.exists():
        git(["clone", "--quiet", "--filter=blob:none", "--bare", repo, str(bare)])
    tree = work / rev[:12]
    if not tree.exists():
        git(["-C", str(bare), "worktree", "add", "--quiet", "--detach", str(tree), rev])
    head = git(["-C", str(tree), "rev-parse", "HEAD"])
    if head != rev:
        raise RuntimeError(f"{tree} is at {head}, expected {rev}")
    return tree


def verify_blobs(tree, rev, blobs, which):
    for blob in blobs:
        got = git(["-C", str(tree), "rev-parse", f"{rev}:{blob['path']}"])
        if got != blob[which]:
            raise RuntimeError(f"{blob['path']} at {rev[:9]} is {got}, expected {blob[which]}")


def binary_tag(trace_bin):
    """A short hash of the binary, so a database is reused only by the
    binary that built it."""
    digest = hashlib.sha256()
    with open(trace_bin, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()[:12]


def analyse(trace_bin, tree, db, inputs):
    if db.exists():
        return
    env = dict(os.environ, TRACE_SOLVE_BUDGET_POPS=str(inputs["budget_pops"]))
    proc = subprocess.run(
        [str(trace_bin), "analyze", str(tree), "-o", str(db), "--jobs", str(inputs["jobs"])],
        text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, env=env)
    if proc.returncode != 0:
        raise RuntimeError(f"trace analyze {tree} failed:\n{proc.stdout[-2000:]}")


def locate(text, pattern):
    """1-based line of the first occurrence of `pattern`, or None."""
    at = text.find(pattern)
    return None if at < 0 else text.count("\n", 0, at) + 1


def slice_json(trace_bin, db, path, line, col, name):
    proc = subprocess.run(
        [str(trace_bin), "inspect", str(db), "slice", "--file", path, "--line", str(line),
         "--col", str(col), "--name", name, "--format", "json"],
        text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if proc.returncode != 0:
        raise RuntimeError(f"slice {path}:{line}:{col} failed: {proc.stderr.strip()}")
    return json.loads(proc.stdout)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--bin", default=str(REPO_ROOT / "target" / "release" / "trace"))
    ap.add_argument("--work", default=str(Path(tempfile.gettempdir()) / "trace_cve_retro"))
    ap.add_argument("--inputs", default=str(REPO_ROOT / "scripts" / "cve_slice_retro.json"))
    ap.add_argument("--revisions", default="pre,post")
    args = ap.parse_args()
    inputs = json.loads(Path(args.inputs).read_text())
    work = Path(args.work)
    work.mkdir(parents=True, exist_ok=True)
    trace_bin = Path(args.bin).resolve()
    tag = binary_tag(trace_bin)
    failures = 0
    for case in inputs["cases"]:
        for which in args.revisions.split(","):
            rev = case[which]
            try:
                tree = ensure_tree(work, inputs["repo"], rev)
                verify_blobs(tree, rev, case["patched_blobs"], which)
                db = work / f"{rev[:12]}-{tag}.db"
                analyse(trace_bin, tree, db, inputs)
            except RuntimeError as err:
                print(f"SETUP  {case['cve']} {which}: {err}")
                return 2
            for start in case["starts"]:
                path = tree / start["file"]
                text = path.read_text(errors="replace")
                line = locate(text, start["pattern"])
                if line is None or (which == "pre" and line != start["pre_line"]):
                    print(f"FAIL  {case['cve']} {which} {start['member']}: start pattern at "
                          f"line {line}, expected {start['pre_line']}")
                    failures += 1
                    continue
                row = text.splitlines()[line - 1]
                col = row.find(start["name"]) + 1
                try:
                    s = slice_json(trace_bin, db, start["file"], line, col, start["name"])
                except RuntimeError as err:
                    print(f"FAIL  {case['cve']} {which} {start['member']}: {err}")
                    failures += 1
                    continue
                starts = set(s["start"]["nodes"])
                flagged = sum(e["cross_context"] for e in s["edges"])
                print(f"  ..  {case['cve']} {which:4} {start['member']}: {len(s['nodes'])} nodes, "
                      f"{len(s['edges'])} edges, {flagged} cross-context, {len(s['contexts'])} contexts")
                for want in start["expect"]:
                    if which not in want.get("revisions", ["pre", "post"]):
                        continue
                    at = locate(text, want["pattern"])
                    if which == "pre" and "pre_line" in want and at != want["pre_line"]:
                        print(f"FAIL    {want['function']}: pattern at line {at}, "
                              f"expected {want['pre_line']}")
                        failures += 1
                        continue
                    hits = [e for e in s["edges"]
                            if e["kind"] == want["kind"] and e["function"] == want["function"]
                            and e["site"] and e["site"]["line"] == at
                            and (e["from"] in starts or e["to"] in starts)]
                    short = want["function"].split("::")[-2:]
                    where = f"{want['kind']} {'::'.join(short)} @{at}"
                    if not want.get("present", True):
                        ok = not hits
                        print(f"{'  ok' if ok else 'FAIL'}    {where}: not in the slice"
                              f"{'' if ok else ' (now present)'}")
                    elif not hits:
                        ok = False
                        print(f"FAIL    {where}: missing")
                    else:
                        got = any(e["cross_context"] for e in hits)
                        ok = got == want["cross_context"]
                        reasons = sorted({r for e in hits for r in e["reasons"]})
                        print(f"{'  ok' if ok else 'FAIL'}    {where}: cross_context={got} "
                              f"{reasons} (expected {want['cross_context']})")
                    failures += not ok
    print(f"{'PASS' if failures == 0 else 'FAIL'}: {failures} failures")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
