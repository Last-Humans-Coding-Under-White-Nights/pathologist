#!/usr/bin/env python3
"""Check out the eval corpora at the revisions pinned in eval_expected.json.

Each corpus entry records `repo` (git URL), `rev` (commit SHA) and `dir`
(checkout directory, relative to the corpus base). The base defaults to
`~`, i.e. the corpora live next to the home directory as before; override
with `--base` or `TRACE_CORPUS_BASE`.

A missing (or empty) directory is shallow-fetched at exactly `rev` (one
commit, no history). An existing checkout is verified against `rev` and
must be clean (`git status --porcelain` empty); pass `--update` to move a
clean checkout to the pinned revision, otherwise a mismatch is an error.
A non-empty directory that is not a git checkout is never touched.

Uses only git and the stdlib.

Usage:
    python3 scripts/fetch_corpora.py [--base DIR] [--expected scripts/eval_expected.json]
        [--update] [corpus ...]
"""

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path

DEFAULT_BASE = "~"


def corpus_base(explicit=None):
    return Path(os.path.expanduser(explicit or os.environ.get("TRACE_CORPUS_BASE", DEFAULT_BASE)))


def corpus_root(spec, base):
    return base / spec["dir"]


def git(args, cwd):
    return subprocess.run(["git", *args], cwd=str(cwd), text=True,
                          stdout=subprocess.PIPE, stderr=subprocess.STDOUT)


def git_output(args, cwd):
    proc = git(args, cwd)
    return proc.stdout.strip() if proc.returncode == 0 else None


def head_rev(root):
    return git_output(["rev-parse", "HEAD"], root)


def local_changes(root):
    """Paths reported by `git status --porcelain` (modified or untracked)."""
    status = git_output(["status", "--porcelain"], root)
    return status.splitlines() if status else []


def is_git_checkout(root):
    return (root / ".git").exists() and head_rev(root) is not None


def fetch_rev(root, repo, rev):
    """Shallow-fetch `rev` from `repo` into `root` and check it out detached."""
    root.mkdir(parents=True, exist_ok=True)
    if not (root / ".git").exists():
        proc = git(["init", "-q"], root)
        if proc.returncode != 0:
            return proc.stdout
    has_origin = git(["remote", "get-url", "origin"], root).returncode == 0
    proc = git(["remote", "set-url" if has_origin else "add", "origin", repo], root)
    if proc.returncode != 0:
        return proc.stdout
    for step in (["fetch", "-q", "--depth", "1", "origin", rev],
                 ["checkout", "-q", "--detach", "FETCH_HEAD"]):
        proc = git(step, root)
        if proc.returncode != 0:
            return proc.stdout
    return None


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--base", help=f"corpus base directory (default $TRACE_CORPUS_BASE or {DEFAULT_BASE})")
    ap.add_argument("--expected", default="scripts/eval_expected.json")
    ap.add_argument("--update", action="store_true",
                    help="move an existing checkout to the pinned revision")
    ap.add_argument("corpora", nargs="*", help="subset of corpora in the expectations file; default all")
    args = ap.parse_args()

    with open(args.expected) as fh:
        expected = json.load(fh)
    base = corpus_base(args.base)
    names = args.corpora or list(expected["corpora"])
    failures = 0
    for name in names:
        spec = expected["corpora"].get(name)
        if spec is None:
            print(f"FAIL  unknown corpus {name!r} (have {list(expected['corpora'])})")
            failures += 1
            continue
        root = corpus_root(spec, base)
        rev = spec["rev"]
        problem = None
        if root.exists() and not is_git_checkout(root) and any(root.iterdir()):
            # Never turn an unrelated directory into a checkout or touch its
            # contents; only an absent or empty directory is fetched into.
            problem = f"{root} exists and is not a git checkout; remove it or point --base elsewhere"
        elif is_git_checkout(root):
            current = head_rev(root)
            dirty = local_changes(root)
            if dirty:
                problem = (f"{root} has local changes ({len(dirty)} paths in "
                           f"`git status --porcelain`); clean or remove it before verifying/updating")
            elif current == rev:
                print(f"  ok  {spec['name']} at {rev[:12]} ({root})")
                continue
            elif not args.update:
                problem = (f"{root} is at {current[:12]}, pinned {rev[:12]} "
                           f"(re-run with --update to move it)")
        if problem:
            print(f"FAIL  {spec['name']}: {problem}")
            failures += 1
            continue
        action = "updating" if is_git_checkout(root) else "fetching"
        print(f"  ..  {action} {spec['name']} at {rev[:12]} -> {root}")
        err = fetch_rev(root, spec["repo"], rev)
        if err is not None or head_rev(root) != rev:
            print(f"FAIL  {spec['name']}: checkout failed\n{err or ''}".rstrip())
            failures += 1
        else:
            print(f"  ok  {spec['name']} at {rev[:12]} ({root})")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
