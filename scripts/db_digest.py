#!/usr/bin/env python3
"""Digest of an analysis database: per table, the row count and the SHA-256
of its rows in rowid order, and one SHA-256 over all of them.

Two runs of `trace analyze` on the same inputs must produce the same digest
(AGENTS.md, invariant 10), whatever the allocator, job count or platform:
this is the comparison docs/EVAL_REPORT.md records as "identical by row
count and SHA-256 of rows in rowid order". `analysis_run` holds run
metadata (`created_at`, the binary's version) and is excluded.

    python3 scripts/db_digest.py /tmp/out.db            # one line per table, then the total
    python3 scripts/db_digest.py --json /tmp/out.db
    python3 scripts/db_digest.py --total /tmp/out.db    # the overall hash only

Uses only the stdlib.
"""
import argparse
import hashlib
import json
import sqlite3
import sys

METADATA_TABLES = {"analysis_run"}


def digest(path):
    """`{"tables": {name: {"rows": n, "sha256": hex}}, "sha256": hex}` for
    every non-metadata table of the database at `path`, tables in name
    order. Rows are hashed as their JSON encoding, one per line, in rowid
    order; a table without a rowid is read in its declared order."""
    con = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    try:
        names = sorted(
            n for (n,) in con.execute(
                "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")
            if n not in METADATA_TABLES)
        tables = {}
        total = hashlib.sha256()
        for name in names:
            h = hashlib.sha256()
            rows = 0
            try:
                cursor = con.execute(f'SELECT * FROM "{name}" ORDER BY rowid')
            except sqlite3.OperationalError:
                cursor = con.execute(f'SELECT * FROM "{name}"')
            for row in cursor:
                h.update(json.dumps(row, ensure_ascii=False).encode("utf-8"))
                h.update(b"\n")
                rows += 1
            tables[name] = {"rows": rows, "sha256": h.hexdigest()}
            total.update(f"{name} {rows} {tables[name]['sha256']}\n".encode("utf-8"))
        return {"tables": tables, "sha256": total.hexdigest()}
    finally:
        con.close()


def render(result):
    """One `<table> <rows> <sha256>` line per table, then `total <sha256>`."""
    width = max((len(n) for n in result["tables"]), default=5)
    lines = [f"{name:{width}s} {t['rows']:>9d} {t['sha256']}"
             for name, t in result["tables"].items()]
    lines.append(f"{'total':{width}s} {'':>9s} {result['sha256']}")
    return "\n".join(lines)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("db")
    fmt = ap.add_mutually_exclusive_group()
    fmt.add_argument("--json", action="store_true", help="print the digest as JSON")
    fmt.add_argument("--total", action="store_true", help="print only the overall SHA-256")
    args = ap.parse_args(argv)
    result = digest(args.db)
    if args.json:
        print(json.dumps(result, indent=1))
    elif args.total:
        print(result["sha256"])
    else:
        print(render(result))
    return 0


if __name__ == "__main__":
    sys.exit(main())
