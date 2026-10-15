#!/usr/bin/env python3
"""Compare indexing on generated or existing projects, checking analysis rows.

Build both release binaries first. Temporary projects and databases are removed
on exit; --output retains measurements and phase logs as JSON.
"""

import argparse
import collections
import itertools
import json
from pathlib import Path
import re
import sqlite3
import statistics
import subprocess
import tempfile
import time


HEADERS = """#include <string>
#include <memory>
#include <vector>
#include <functional>
#include <map>
#include <algorithm>
"""


def make_project(root, shape, units):
    project = root / shape
    project.mkdir()
    if shape == "shared":
        (project / "common.h").write_text("#pragma once\n" + HEADERS)
    for i in range(units):
        includes = HEADERS
        if shape == "shared":
            includes = '#include "common.h"\n'
        elif shape == "unique":
            (project / f"h{i}.h").write_text(
                "#pragma once\n" + HEADERS + f"struct Value{i} {{ int value; }};\n"
            )
            includes = f'#include "h{i}.h"\n'
        elif shape == "middle":
            # A different declaration before installed headers, plus a
            # different function after them, defeats prefix/suffix-only reuse.
            includes = f"struct Prefix{i} {{ int value; }};\n" + HEADERS
        elif shape == "initializers":
            # Repeatedly saving initializer origins must not rescan the
            # large LineMap prefix contributed by installed headers.
            fields = "".join(f"int field{j} = {j};\n" for j in range(512))
            includes += f"struct Defaults{i} {{\n{fields}}};\n"
        elif shape == "templates":
            # Each return checks the same class aliases and template names;
            # class members must not all be scanned again for every method.
            methods = "".join(
                f"Pointer method{j}(int a, int b, int c, int d);\n"
                for j in range(256)
            )
            includes += (
                f"template<class T> struct Template{i} {{\n"
                "using Value = T; using Pointer = Value*;\n" + methods + "};\n"
            )
        (project / f"u{i}.cpp").write_text(
            includes + f'void run{i}() {{ std::string text("example"); '
            "std::vector<int> values; values.push_back(text.size()); }\n"
        )
    return project


def quote(name):
    return '"' + name.replace('"', '""') + '"'


def compare_databases(before, after):
    # Compare underlying tables, including IDs, diagnostics and source spans.
    # Only the run metadata (timestamps/output paths) may differ.
    left = sqlite3.connect(f"file:{before}?mode=ro", uri=True)
    right = sqlite3.connect(f"file:{after}?mode=ro", uri=True)
    try:
        tables_sql = (
            "SELECT name FROM sqlite_master WHERE type='table' "
            "AND name NOT LIKE 'sqlite_%' AND name <> 'analysis_run' ORDER BY name"
        )
        tables = left.execute(tables_sql).fetchall()
        if tables != right.execute(tables_sql).fetchall():
            raise RuntimeError("analysis table sets differ")
        total = 0
        for (table,) in tables:
            columns = left.execute(f"PRAGMA table_info({quote(table)})").fetchall()
            if columns != right.execute(f"PRAGMA table_info({quote(table)})").fetchall():
                raise RuntimeError(f"{table}: schemas differ")
            keys = [c[1] for c in sorted(columns, key=lambda c: c[5]) if c[5]]
            order = keys or [c[1] for c in columns]
            sql = f"SELECT * FROM {quote(table)} ORDER BY " + ",".join(map(quote, order))
            for a, b in itertools.zip_longest(left.execute(sql), right.execute(sql)):
                if a != b:
                    raise RuntimeError(f"{table}: different rows\nbefore: {a}\nafter: {b}")
                total += 1
        return total
    finally:
        left.close()
        right.close()


def semantic_rows(path):
    """Normalize exported behavior when shared units change entity numbering.

    This deliberately excludes unused dependency declarations and metadata.
    Use exact comparison to check determinism of a single implementation.
    """
    with sqlite3.connect(f"file:{path}?mode=ro", uri=True) as db:
        files = dict(db.execute("SELECT id,path FROM files"))
        functions = {
            row[0]: (row[1], files[row[2]], *row[3:])
            for row in db.execute(
                "SELECT id,name,file_id,line_start,signature,is_defined,is_dep,is_weak "
                "FROM functions"
            )
        }
        sites = {
            row[0]: (functions[row[1]], files[row[2]], row[3], row[4],
                     files.get(row[5]), *row[6:])
            for row in db.execute(
                "SELECT id,caller_fn_id,file_id,line,col,expansion_file_id,"
                "expansion_line,expansion_col,callee_text,is_direct FROM call_sites"
            )
        }
        params = {
            var: (functions[fn], index)
            for fn, index, var in db.execute(
                "SELECT fn_id,arg_index,var_id FROM flow_parameters_data"
            )
        }
        variables = {
            var: ("param", *params[var]) if var in params else (
                "var", re.sub(r"\d+$", "", name) if synthetic else name,
                kind, functions.get(fn), files.get(file), line, col,
            )
            for var, name, kind, fn, file, line, col, synthetic in db.execute(
                "SELECT id,name,kind,fn_id,file_id,line,col,is_synthetic FROM variables"
            )
        }
        nodes = {
            node: (kind, variables.get(var), functions.get(fn), sites.get(site),
                   label if kind != "var" else "", detail)
            for node, kind, label, detail, var, fn, site in db.execute(
                "SELECT id,kind,label,detail,var_id,fn_id,call_site_id FROM flow_nodes"
            )
        }
        return {
            "definitions": collections.Counter(
                key for key in functions.values() if key[-3] and not key[-2]
            ),
            "sites": collections.Counter(sites.values()),
            "calls": collections.Counter(
                (sites.get(site), functions[caller], functions[callee], kind)
                for site, caller, callee, kind in db.execute(
                    "SELECT call_site_id,caller_fn_id,callee_fn_id,resolution FROM call_edges"
                )
            ),
            "arguments": collections.Counter(
                (sites[site], index, variables.get(actual), functions.get(actual_fn),
                 variables.get(formal))
                for site, index, actual, actual_fn, formal in db.execute(
                    "SELECT call_site_id,arg_index,actual_var_id,actual_fn_id,formal_var_id "
                    "FROM arg_flow_edges"
                )
            ),
            "flow": collections.Counter(
                (nodes[src], nodes[dst], kind)
                for src, dst, kind in db.execute("SELECT src_node,dst_node,kind FROM flow_edges")
            ),
            "origins": collections.Counter(
                (kind, files[file], line, col, expr, op)
                for kind, file, line, col, expr, op in db.execute(
                    "SELECT kind,file_id,line,col,expression,operation FROM flow_origins"
                )
            ),
        }


def compare_semantics(before, after):
    left, right = semantic_rows(before), semantic_rows(after)
    for kind, rows in left.items():
        if rows != right[kind]:
            missing, added = rows - right[kind], right[kind] - rows
            raise RuntimeError(
                f"{kind}: {missing.total()} missing, {added.total()} added\n"
                f"missing example: {next(iter(missing), None)!r}\n"
                f"added example: {next(iter(added), None)!r}"
            )
    return sum(rows.total() for rows in left.values())


def positive(value):
    value = int(value)
    if value < 1:
        raise argparse.ArgumentTypeError("must be positive")
    return value


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, default=Path("target/release/trace"))
    parser.add_argument("--units", type=positive, default=20)
    parser.add_argument("--repeats", type=positive, default=3)
    parser.add_argument("--jobs", type=positive, default=1)
    parser.add_argument("--project", type=Path,
                        help="benchmark an existing project instead of generated layouts")
    parser.add_argument("--system-includes", action=argparse.BooleanOptionalAction, default=True,
                        help="also supports --no-system-includes for default-mode regressions")
    parser.add_argument("--comparison", choices=["exact", "semantic"], default="exact",
                        help="semantic allows reordered IDs and unused dependency metadata")
    parser.add_argument("--layouts", nargs="+",
                        choices=["direct", "middle", "initializers", "templates", "shared", "unique"],
                        default=["direct", "shared", "unique"])
    parser.add_argument("--timeout", type=positive, default=180)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    binaries = {"baseline": args.baseline.resolve(), "candidate": args.candidate.resolve()}
    for binary in binaries.values():
        if not binary.is_file():
            parser.error(f"binary does not exist: {binary}")
    results = []
    with tempfile.TemporaryDirectory(prefix="trace-system-bench-") as directory:
        root = Path(directory)
        projects = [("project", args.project.resolve())] if args.project else [
            (shape, make_project(root, shape, args.units)) for shape in args.layouts
        ]
        for shape, project in projects:
            reference = root / f"{shape}-baseline-0.db"
            for repeat in range(args.repeats):
                # Alternate ordering to reduce a fixed warm-cache/order bias.
                order = list(binaries) if repeat % 2 == 0 else list(reversed(binaries))
                for label in order:
                    database = root / f"{shape}-{label}-{repeat}.db"
                    start = time.monotonic()
                    command = [str(binaries[label]), "analyze", str(project),
                         "--jobs", str(args.jobs), "--timeout-secs", str(args.timeout),
                         "-o", str(database)]
                    if args.system_includes:
                        command.append("--system-includes")
                    run = subprocess.run(
                        command,
                        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
                        timeout=args.timeout + 30,
                    )
                    seconds = time.monotonic() - start
                    if run.returncode:
                        raise RuntimeError(f"{shape}/{label}: exit {run.returncode}\n{run.stdout}")
                    compare = compare_databases if args.comparison == "exact" else compare_semantics
                    rows = compare(reference, database)
                    result = dict(shape=shape, variant=label, repeat=repeat, seconds=seconds,
                                  units=None if args.project else args.units, jobs=args.jobs,
                                  project=str(project), system_includes=args.system_includes,
                                  comparison=args.comparison,
                                  identical_rows=rows,
                                  phases=run.stdout.splitlines())
                    results.append(result)
                    print(f"{shape}/{label} #{repeat + 1}: {seconds:.3f}s; "
                          f"{rows} identical {args.comparison} rows", flush=True)
                    if args.output:
                        args.output.write_text(json.dumps(results, indent=2) + "\n")
            for label in binaries:
                samples = [r["seconds"] for r in results if r["shape"] == shape and r["variant"] == label]
                print(f"{shape}/{label} median: {statistics.median(samples):.3f}s", flush=True)


if __name__ == "__main__":
    main()
