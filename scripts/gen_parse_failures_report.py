#!/usr/bin/env python3
"""Render a parse-failure snapshot; see docs/PARSE_FAILURES.md for capture instructions."""

from __future__ import annotations

import argparse
import os
import re
import sys
from collections import Counter, defaultdict
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
GUIDE = REPO / "docs" / "PARSE_FAILURES.md"

# Directory holding checkouts of the eval corpora (one subdirectory per
# corpus, named as below) at the revisions pinned in scripts/eval_expected.json
# — `python3 scripts/fetch_corpora.py` produces exactly this layout. Same
# convention as eval_check.py: $TRACE_CORPUS_BASE, default ~
# (PARSE_CORPUS_BASE is still honoured).
CORPUS_ENV = "TRACE_CORPUS_BASE"

# Record kinds examples/parse_failures.rs emits; anything else means the
# TSV did not come from that tool (or came from a different version).
TSV_KINDS = {"ERROR", "PARSE", "PREPROCESS"}
CORPUS_BASE = Path(
    os.path.expanduser(
        os.environ.get(CORPUS_ENV) or os.environ.get("PARSE_CORPUS_BASE") or "~"
    )
)

CORPORA = [
    {
        "id": "hdf",
        "name": "drivers_hdf_core",
        "root": CORPUS_BASE / "drivers_hdf_core",
        "tsv": Path("/tmp/parse_failures_hdf.tsv"),
    },
    {
        "id": "hiview",
        "name": "hiviewdfx_hiview",
        "root": CORPUS_BASE / "hiviewdfx_hiview",
        "tsv": Path("/tmp/parse_failures_hiview.tsv"),
    },
    {
        "id": "camera",
        "name": "multimedia_camera_framework",
        "root": CORPUS_BASE / "multimedia_camera_framework",
        "tsv": Path("/tmp/parse_failures_camera.tsv"),
    },
]


def categorize_reason(reason: str) -> str:
    r = reason.strip()
    if "extern template" in r or "template declaration or instantiation" in r:
        return "template declarations and instantiations"
    if "IDL/interface macro" in r or "dotted interface name" in r:
        return "IDL dotted interface names"
    if "optional parameter" in r:
        return "default function parameters"
    if "missing ;" in r:
        return "missing semicolons"
    if "missing type_identifier" in r:
        return "missing type identifiers"
    if "generic tree-sitter ERROR" in r:
        return "generic ERROR nodes"
    if "operator" in r:
        return "operator overload syntax"
    if "preprocess failed" in r:
        return "preprocess failure"
    return "other / mixed"


def summarize(errs: list[dict], note: str | None) -> str:
    if note and note.startswith("preprocess"):
        return note
    if not errs:
        return note or "tree-sitter parse tree contains errors (site not localized)"
    nodes = [e["node"] for e in errs]
    if any("parenthesized_declarator" in n for n in nodes) and any(
        "." in e["snippet"] for e in errs
    ):
        return (
            "possible dotted interface name in a parenthesized declarator "
            "(inspect the source and macro expansion)"
        )
    if any(
        "extern template" in e["snippet"] or e["snippet"].startswith("template ")
        for e in errs
    ):
        return (
            "error near a template declaration or instantiation "
            "(inspect the source and macro expansion)"
        )
    if any("optional_parameter_declaration" in n for n in nodes):
        return "default function parameters with complex types (optional parameter declaration)"
    if any(n == "ERROR" for n in nodes):
        return "generic tree-sitter ERROR node(s) in preprocessed source"
    if any("operator" in e["snippet"] for e in errs):
        return "C++ operator overload syntax"
    if any("decltype" in e["snippet"] or "auto" in e["snippet"] for e in errs):
        return "C++11+ type syntax (auto/decltype) in declaration"
    top = Counter(nodes).most_common(1)[0][0]
    return f"tree-sitter node `{top}` at {len(errs)} site(s)"


def load_tsv(tsv: Path, root: Path) -> dict[str, dict]:
    # The completion record distinguishes zero failures from a failed or
    # truncated capture. The recipe also renames `.part` files only on success.
    if not tsv.exists():
        sys.exit(
            f"{tsv} does not exist -- regenerate it (see the recipe at the top "
            f"of {GUIDE.relative_to(REPO)}); refusing to write a partial report"
        )
    with tsv.open(encoding="utf-8", newline="") as source:
        text = source.read()
    # Read exactly the producer's newline-separated records. Other line
    # separators inside snippets are data, not additional rows.
    if not text or not text.endswith("\n"):
        sys.exit(
            f"{tsv}: does not end in a newline, so it was truncated mid-write "
            f"-- regenerate it; refusing to write a partial report"
        )
    rows = text[:-1].split("\n")
    footer = rows.pop().split("\t")
    if len(footer) != 2 or footer[0] != "END" or footer[1] != str(len(rows)):
        sys.exit(f"{tsv}: missing or mismatched completion record; regenerate it")
    files: dict[str, dict] = defaultdict(lambda: {"errors": [], "note": None})
    for lineno, line in enumerate(rows, 1):
        # Every row is FILE<tab>path<tab>KIND<tab>detail. maxsplit=3 preserves
        # any tabs in detail; foreign rows must not become a partial report.
        parts = line.split("\t", 3)
        if len(parts) != 4 or parts[0] != "FILE" or parts[2] not in TSV_KINDS:
            sys.exit(
                f"{tsv}:{lineno}: malformed row, refusing to write a partial "
                f"report -- expected FILE<tab>path<tab>"
                f"{{{'|'.join(sorted(TSV_KINDS))}}}<tab>detail, got:\n  {line!r}"
            )
        _, path, kind, detail = parts
        if not path:
            sys.exit(f"{tsv}:{lineno}: empty source path; regenerate it")
        try:
            rel = str(Path(path).relative_to(root))
        except ValueError:
            # External headers can share a basename: keep their full identity.
            rel = path
        if kind == "ERROR":
            # Node kinds can contain parentheses (e.g. "missing )"). Use
            # the first closing ") " delimiter so snippet parentheses stay data.
            m = re.fullmatch(r"line (\d+) col (\d+) \((.+?)\) (.*)", detail)
            if m:
                files[rel]["errors"].append(
                    {
                        "line": int(m.group(1)),
                        "col": int(m.group(2)),
                        "node": m.group(3),
                        "snippet": m.group(4),
                    }
                )
            else:
                sys.exit(
                    f"{tsv}:{lineno}: malformed ERROR detail; regenerate it"
                )
        elif kind == "PARSE":
            files[rel]["note"] = detail
        elif kind == "PREPROCESS":
            files[rel]["note"] = "preprocess failed: " + detail
    return dict(files)


def category_counts(files: dict[str, dict]) -> Counter[str]:
    counts: Counter[str] = Counter()
    for rel, info in files.items():
        reason = summarize(info["errors"], info.get("note"))
        counts[categorize_reason(reason)] += 1
    return counts


def render_corpus(corpus: dict, files: dict[str, dict]) -> list[str]:
    lines: list[str] = []
    name = corpus["name"]
    root = corpus["root"]
    lines.append(f"## {name}")
    lines.append("")
    lines.append(
        f"Diagnostic preprocessing of `{root}` "
        f"({len(files)} files with reported failures)."
    )
    lines.append(
        "Each entry is a source file selected for diagnostic preprocessing; "
        "reasons come from parser nodes or preprocessing failures."
    )
    lines.append("")
    lines.append(f"**Total failing files:** {len(files)}")
    lines.append("")

    cats = category_counts(files)
    lines.append("### Failure categories")
    lines.append("")
    lines.append("| Category | Files |")
    lines.append("|----------|------:|")
    for cat, n in cats.most_common():
        lines.append(f"| {cat} | {n} |")
    lines.append("")

    lines.append("### File list")
    lines.append("")
    lines.append("| # | File | Reason | Error sites |")
    lines.append("|---|------|--------|-------------|")
    for i, rel in enumerate(sorted(files.keys()), 1):
        info = files[rel]
        reason = summarize(info["errors"], info.get("note"))
        sites = len(info["errors"]) if info["errors"] else "—"
        lines.append(f"| {i} | `{rel}` | {reason} | {sites} |")
    lines.append("")

    lines.append("### Per-file details")
    lines.append("")
    for rel in sorted(files.keys()):
        info = files[rel]
        lines.append(f"#### `{rel}`")
        lines.append("")
        lines.append(f"**Summary:** {summarize(info['errors'], info.get('note'))}")
        lines.append("")
        if info["errors"]:
            lines.append("| Line | Col | Node kind | Snippet |")
            lines.append("|-----:|----:|-----------|---------|")
            for e in sorted(info["errors"], key=lambda x: (x["line"], x["col"]))[:20]:
                snip = e["snippet"].replace("|", "\\|")
                lines.append(
                    f"| {e['line']} | {e['col']} | `{e['node']}` | `{snip}` |"
                )
            if len(info["errors"]) > 20:
                lines.append(
                    f"| … | … | … | *({len(info['errors']) - 20} more)* |"
                )
        elif info.get("note"):
            lines.append(f"Note: {info['note']}")
        lines.append("")

    return lines


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=Path("/tmp/parse_failures_report.md"))
    args = parser.parse_args()
    parsed = []
    for corpus in CORPORA:
        files = load_tsv(corpus["tsv"], corpus["root"])
        parsed.append((corpus, files))

    out: list[str] = []
    out.append("# Parse failures — eval corpora")
    out.append("")
    out.append(
        "Parse failures reported by the diagnostic example. "
        "Generated from locally captured TSV files."
    )
    out.append("")
    out.append("Capture instructions and limitations: `docs/PARSE_FAILURES.md`.")
    out.append("")
    out.append("## Overview")
    out.append("")
    out.append("| Corpus | Root | Failing files | Top category |")
    out.append("|--------|------|--------------:|--------------|")
    for corpus, files in parsed:
        cats = category_counts(files)
        top = cats.most_common(1)[0][0] if cats else "—"
        out.append(
            f"| `{corpus['name']}` | `{corpus['root']}` | {len(files)} | {top} |"
        )
    out.append("")
    out.append("## Cross-corpus category totals")
    out.append("")
    merged: Counter[str] = Counter()
    for _, files in parsed:
        merged.update(category_counts(files))
    out.append("| Category | HDF | Hiview | Camera | Total |")
    out.append("|----------|----:|-------:|-------:|------:|")
    all_cats = sorted(
        set(merged),
        key=lambda c: (-merged[c], c),
    )
    per_corpus_cats = [category_counts(files) for _, files in parsed]
    for cat in all_cats:
        hdf, hiview, camera = (c.get(cat, 0) for c in per_corpus_cats)
        out.append(f"| {cat} | {hdf} | {hiview} | {camera} | {merged[cat]} |")
    out.append("")

    for corpus, files in parsed:
        out.extend(render_corpus(corpus, files))
        out.append("---")
        out.append("")

    args.output.write_text("\n".join(out).rstrip() + "\n", encoding="utf-8")
    print(f"Wrote {args.output} ({len(parsed)} corpora, {sum(len(f) for _, f in parsed)} files)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
