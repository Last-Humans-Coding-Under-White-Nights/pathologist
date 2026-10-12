#!/usr/bin/env python3
"""Compare all logical analysis rows, including duplicates, across v7/v8 exports.

Run/build both analyzers with identical inputs and options first, then:
  python3 scripts/compare_analysis_databases.py before.db after.db --output comparison.json

Excludes creation time, trace build identity and schema version, and compares
views instead of their compact backing tables/dictionary. Streams sorted rows
to keep Python memory bounded on large corpora. Exit 1 indicates a difference.
"""
import argparse
from contextlib import closing
from itertools import zip_longest
import json
from pathlib import Path
import sqlite3

from analysis_sqlite import logical_relations, relation_query


def compare_databases(before, after):
    before, after = Path(before), Path(after)
    with closing(sqlite3.connect(before.resolve().as_uri() + "?mode=ro", uri=True)) as old, \
            closing(sqlite3.connect(after.resolve().as_uri() + "?mode=ro", uri=True)) as new:
        prior, current = set(logical_relations(old)), set(logical_relations(new))
        report = {"bytes_before": before.stat().st_size, "bytes_after": after.stat().st_size,
                  "equal": True, "relations": {}}
        absent = object()
        for relation in sorted(prior | current):
            if relation not in prior or relation not in current:
                result = {"equal": False, "missing": "before" if relation not in prior else "after"}
            else:
                before_columns, before_query = relation_query(old, relation)
                after_columns, after_query = relation_query(new, relation)
                if before_columns != after_columns:
                    result = {"equal": False, "columns_before": before_columns,
                              "columns_after": after_columns}
                else:
                    result = {"equal": True, "rows_before": 0, "rows_after": 0}
                    for left, right in zip_longest(old.execute(before_query), new.execute(after_query),
                                                  fillvalue=absent):
                        result["rows_before"] += left is not absent
                        result["rows_after"] += right is not absent
                        if left != right and result["equal"]:
                            result.update(equal=False,
                                          first_difference={"before": None if left is absent else left,
                                                            "after": None if right is absent else right})
            report["relations"][relation] = result
            report["equal"] &= result["equal"]
        return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before", type=Path)
    parser.add_argument("after", type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    report = compare_databases(args.before, args.after)
    output = json.dumps(report, indent=2) + "\n"
    if args.output:
        args.output.write_text(output)
    print(output, end="")
    return 0 if report["equal"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
