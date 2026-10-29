#!/usr/bin/env python3
"""Compare source-keyed direct edges and classify receiver-recovery evidence.
This is an evidence audit, not a ground-truth precision percentage.
"""
import argparse
import collections
import json
import sqlite3
from pathlib import Path

SQL = '''SELECT caller.name, cf.path, caller.line_start, sf.path, cs.line, cs.col,
                callee.name, tf.path, callee.line_start, e.resolution, callee.is_defined,
                callee.signature, cs.callee_text
         FROM call_edges e JOIN functions caller ON caller.id=e.caller_fn_id
         JOIN files cf ON cf.id=caller.file_id JOIN functions callee ON callee.id=e.callee_fn_id
         JOIN files tf ON tf.id=callee.file_id LEFT JOIN call_sites cs ON cs.id=e.call_site_id
         LEFT JOIN files sf ON sf.id=cs.file_id
         ORDER BY caller.name, cf.path, caller.line_start, sf.path, cs.line, cs.col, callee.name, tf.path, callee.line_start'''


def read(path):
    with sqlite3.connect(path) as c:
        return list(c.execute(SQL))


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('baseline', type=Path)
    ap.add_argument('candidate', type=Path)
    ap.add_argument('--samples', type=int, default=12)
    ap.add_argument('--output', type=Path)
    args = ap.parse_args()
    before, after = read(args.baseline), read(args.candidate)
    # Source identity is independent of exported entity and dictionary IDs.
    def key(r):
        return r[:9]
    old = collections.Counter(key(r) for r in before if r[9] == 'direct')
    new = collections.Counter(key(r) for r in after if r[9] == 'direct')
    added, removed = new-old, old-new
    external = collections.defaultdict(list)
    for r in before:
        if r[9] == 'external':
            external[r[:6]].append(r[6])
    groups = collections.Counter()
    samples = collections.defaultdict(list)
    for row in after:
        if row[9] != 'direct' or not added[key(row)]:
            continue
        added[key(row)] -= 1
        target = row[6]
        owner, _, method = target.rpartition('::')
        if owner.rsplit('::', 1)[-1] == method:
            group = 'constructor'
        else:
            leaves = [name for name in external[row[:6]] if name.rsplit('::', 1)[-1] == method]
            matching = [name.rpartition('::')[0] for name in leaves if name.rpartition('::')[0].rsplit('::',1)[-1] == owner.rsplit('::',1)[-1]]
            if not matching:
                group = 'other_resolution'
            elif any('::' not in candidate for candidate in matching):
                group = 'bare_owner'
            else:
                ns = owner.rpartition('::')[0]
                owners = [candidate.rpartition('::')[0] for candidate in matching]
                if any(ns == other or ns.startswith(other+'::') or other.startswith(ns+'::') for other in owners):
                    group = 'same_or_nested_namespace'
                elif any(ns.split('::')[0] == other.split('::')[0] for other in owners):
                    group = 'shared_enclosing_namespace'
                else:
                    group = 'using_evidence_to_inspect'
        groups[group] += 1
        if len(samples[group]) < args.samples:
            samples[group].append({'caller':row[0], 'file':row[3], 'line':row[4], 'callee':target,
                                   'signature':row[11], 'external_leaves':external[row[:6]]})
    report = {'added_direct':sum((new-old).values()), 'removed_direct':sum(removed.values()),
              'groups':dict(sorted(groups.items())), 'samples':dict(samples)}
    text = json.dumps(report,indent=2)
    if args.output:
        args.output.write_text(text+'\n')
    print(text)


if __name__ == '__main__':
    main()
